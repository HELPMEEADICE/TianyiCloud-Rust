# 云盘网络挂载（零磁盘文件缓存）

把天翼云盘挂成操作系统的原生文件系统，Windows 上是**真正的网络驱动器**（`GetDriveTypeW == DRIVE_REMOTE`），
Linux 上是只读 FUSE 挂载。整个功能最硬的约束只有一条：

> **读取云端文件内容时，一个字节都不写本地磁盘。**

---

## 1. 它是什么 / 不是什么

是：

* Windows：`X:\` 这样的盘符 + `\\TianyiCloud-<账号>\Personal` 这样的 UNC 路径，两者指向同一个
  WinFsp 网络文件系统；资源管理器"此电脑"里归入网络驱动器，卷标显示为 `天翼云盘 (X:)`。
* Linux：`~/TianyiCloud` 之类的目录，只读 FUSE 挂载。
* 随机读：视频拖进度条、PDF 翻页、图片查看、压缩包索引，都走按需 HTTP Range。

不是：

* 不是 SMB/局域网共享。UNC 前缀是本机的网络提供程序（WinFsp.Np）解析的，**不向局域网提供任何服务**。
* 不是本地磁盘伪装。盘符类型由驱动决定（`WinFsp.Net` 设备），改图标/写注册表/建快捷方式都做不到这件事。
* 不是"读一次缓存在本地"的加速盘。没有磁盘缓存，见第 3 节。

---

## 2. 分层

| 层 | 位置 | 职责 |
|----|------|------|
| 读取 | `crates/tianyi-mount/src/reader.rs` | 严格 Range 校验、有界重试、直链刷新、并发闸门 |
| 元数据 | `src/meta.rs` | 目录缓存（TTL + 容量）、时间戳换算、显示名去重 |
| 路径 | `src/paths.rs` | Windows 非法字符/保留名/超长名/大小写冲突/`..` 穿越 |
| 语义 | `src/fs.rs` | 路径逐段解析、直链缓存、只读约束、云端变更失效 |
| 会话 | `src/session.rs` | 独立的 `TianyiClient`（不跟 GUI 的空间切换打架），失效刷新并回写 |
| 生命周期 | `src/mount.rs` | 挂载点校验、UNC 生成、状态快照、幂等卸载 |
| Windows | `src/win/` | 手写 WinFsp FFI + 网络文件系统回调 |
| Linux | `src/fuse_linux.rs` | fuser 只读文件系统 |

前端（GUI / CLI）只跟 `mount::prepare` + `mount::mount` + `MountHandle` 打交道。

---

## 3. 零磁盘文件缓存契约

实现侧的承诺：

1. **没有**临时文件、`.part`、块缓存目录、SQLite 内容缓存、隐藏缓存文件。
2. 打开文件**不会**触发整文件下载。每次读取都是 `Range: bytes=offset-end`。
3. 服务端忽略 Range 返回 `200 OK` 时**直接判错**（`Error::RangeUnsupported`），
   绝不"读完整个响应再截取"——那正是零缓存模式下的禁忌。
4. 响应必须满足：`206` + `Content-Range` 起点等于请求起点 + `Content-Range` 总长等于本地记录的文件大小，
   否则报 `RangeMismatch`；短读、`416`、EOF 各有明确处置。
5. 内存有上界：并发数 × 单次长度（默认 4 × 8 MiB = 32 MiB 上限，可在 `ReadLimits` 调整），
   响应体边收边计数，超过上限立刻中止。
6. 预读默认**关闭**（`readahead = 0`）；开启时只是在同一次请求里拉长，不额外发请求。
7. 静态审计测试（`tests/no_disk_cache_audit.rs`）扫描本 crate 产品代码，
   出现 `File::create` / `OpenOptions` / `std::fs::write` / `tempfile` / `.part` / `rusqlite` / `write_all(`
   等模式即测试失败。

**不在承诺范围内的磁盘写入**（不受本程序控制，也不做任何声称）：

* 操作系统分页文件 / 内存交换；
* Windows 搜索索引、缩略图缓存、杀毒软件、备份软件对被读数据的二次落盘；
* 用户自己用资源管理器"复制/另存为"产生的目标文件；
* 程序自己的**配置与状态**（`config.json` / `accounts.json` / `mounts.json`）——那是账号与挂载状态，
  不是云端文件内容。

---

## 4. Windows：为什么它是"网络驱动器"

WinFsp 有两条控制设备：`WinFsp.Disk`（本地盘语义）与 `WinFsp.Net`（网络盘语义）。
判定规则很朴素：**`VolumeParams.Prefix` 非空就选 `WinFsp.Net`**。

因此 `crates/tianyi-mount/src/win/mod.rs` 里：

* `VolumeParams.Prefix = \TianyiCloud-<清洗过的账号>\Personal`（个人云）或 `...\Family`（家庭云）；
* `DevicePath = "WinFsp.Net"`；
* `VolumeParams.Flags` 置 `ReadOnlyVolume | UnicodeOnDisk | CasePreservedNames | AlwaysUseDoubleBuffering`；
* `FspFileSystemSetMountPoint("X:")` 建盘符；
* WinFsp 自带的 `WinFsp.Np` 网络提供程序（注册在 `NetworkProvider\Order` 首位）负责把
  `\\<Server>\<Share>` 解析到同一个卷。

于是 `GetDriveTypeW("X:\\")` 返回 `DRIVE_REMOTE(4)`。这不是外观修饰，改不了，也不需要改。

### 为什么手写 FFI 而不是用 `winfsp` crate

现成的 `winfsp` crate 要求 MSVC + bindgen(libclang) + WinFsp SDK（`inc/` + `lib/`），
而实际机器上常常只装了 WinFsp **运行时**。本实现：

* 手写 `src/win/sys.rs`，结构体布局逐个对照 WinFsp 2.1 头文件，并用
  `const _: () = assert!(size_of::<VolumeParams>() == 504)` 之类把 ABI 钉死；
* `LoadLibraryW` + `GetProcAddress` 运行时解析 12 个函数（先读注册表 `InstallDir`，
  再试 `Program Files*\WinFsp\bin`，最后交给系统搜索）；
* 结果：不需要 SDK、不需要 import lib、不需要 clang，MSVC 与 GNU 工具链都能编；
* WinFsp 没装时是**可诊断的错误**（带 winfsp.dev 安装指引），不是加载期崩溃。

### 只读语义

* `ReadOnlyVolume = 1`，写请求在驱动侧就被拦；
* 回调侧再拦一遍：`Write/Create/Overwrite/SetBasicInfo/SetFileSize/SetDelete/CanDelete/Rename/SetSecurity`
  返回 `STATUS_MEDIA_WRITE_PROTECTED` / `STATUS_CANNOT_DELETE`；
* `Open` 只在**请求了写权限**时拒绝（`FILE_WRITE_DATA | APPEND | WRITE_EA | WRITE_ATTRIBUTES | DELETE | WRITE_DAC | WRITE_OWNER`），
  `FILE_READ_DATA / READ_ATTRIBUTES / SYNCHRONIZE` 一律放行——很多普通程序打开文件时会顺手要几个无关权限，
  一刀切会让"只读都读不了"。

### 其它文件系统语义

* 大小写不敏感查找（云端区分大小写，Windows 不区分）：目录条目在构造时建立小写索引；
* 非法字符/保留设备名（CON、COM1…）/超长组件（>200 UTF-16）/重名（仅大小写不同）：
  统一清洗 + 确定性去重（`name (1).ext`），且**同一份云端数据每次得到同样的名字**；
* 时间戳：云端给的是 UTC+8 无时区字符串，按 UTC+8 解释后转 FILETIME；
* 目录分页：`FspFileSystemAcquire/Fill/Read/ReleaseDirectoryBuffer`，marker 由 WinFsp 维护；
* 目录缓存 1 秒后云端的改动就能反映出来（`FileInfoTimeout = 1000`）。

---

## 5. 安装要求

### Windows

1. 安装 **WinFsp 2.x**：<https://winfsp.dev/rel/>（运行时即可，不需要 SDK）。
2. 安装后确认服务在跑：`sc query WinFsp.Launcher`；
   网络提供程序：`reg query "HKLM\SYSTEM\CurrentControlSet\Control\NetworkProvider\Order"`
   应包含 `WinFsp.Np`。
3. 挂载需要普通用户权限（不需要管理员）。

### Linux

* `fuse3`（`/dev/fuse` 存在），当前用户在 `fuse` 组里；
* 构建时需要 `pkg-config` 与 libfuse 开发包（fuser 的依赖）。

---

## 6. 使用方法

### CLI

```text
tianyi-app -c mount X: --read-only --no-disk-cache          # Windows：前台挂载，Ctrl-C 卸载
tianyi-app -c mount ~/TianyiCloud --read-only --no-disk-cache
tianyi-app -c mount X: --space family --unc-prefix '\TianyiCloud\Family'
tianyi-app -c mounts            # 列出当前挂载
tianyi-app -c mounts --json     # 机器可读
tianyi-app -c unmount X:        # 控制另一个进程卸载
```

（具体参数以 `tianyi-app -c mount --help` 为准。）

### GUI

主界面新增「云盘挂载」页：选空间、选盘符、看只读/零缓存状态、挂载/卸载、
实时读取速度与累计流量、UNC 路径、"在资源管理器中打开"、错误提示。
WinFsp 缺失时挂载按钮禁用并显示安装指引。

### 库

```text
let store = ConfigStore::open().0;
let options = MountOptions { mount_point: "X:".into(), ..Default::default() };
let (_session, fs) = tianyi_mount::mount::prepare(store, &options)?;
let handle = tianyi_mount::mount::mount(fs, options)?;
println!("{}", serde_json::to_string(&handle.status())?);
handle.unmount()?;
```

---

### 已知缺陷（真机实测，未修复）

**另一个进程递归进入子目录**时会出问题：

* 现象：挂载后从**别的进程**枚举根目录一切正常（`cmd /c dir X:\`、PowerShell `Get-ChildItem X:\` 都能列出 11 项、大小与时间正确，卷标/序列号正常）；
  但再枚举**子目录**（`Get-ChildItem X:\<子目录>`）会返回空列表，第二次这样的请求会让挂载进程以
  `0xC0000374`（STATUS_HEAP_CORRUPTION）退出。**在挂载进程内部**递归枚举（`netdrive_probe` 的
  子目录用例）则一切正常。
* 已排除：目录缓冲辅助函数（已改成手工分页、零跨调用状态）、8 字节对齐、双缓冲开关、
  `PostCleanupWhenModifiedOnly`、Close 时释放目录缓冲、trace 插桩本身的误导（早期的插桩因正则
  串到了相邻函数，日志里的函数名不可信——这点也记录下来避免后来者被同一日志误导）。
* 影响：资源管理器/命令行**浏览盘符根目录正常**，双击进入子文件夹会触发该缺陷。
* 处置：如实标注为已知缺陷。下一步该做的是在挂载进程内装上 vectored exception handler
  + minidump 抓调用栈，再按栈定位（当前环境没有管理员权限、也无法写 HKLM 的 WER LocalDumps）。

**环境副作用**：修复前的一次调试运行被工具超时杀掉，留下一个陈旧的 WinFsp 卷
（`fsptool lsvol` 里能看到 `X:`），它会占用同名 UNC 前缀，导致不带 `--unc-prefix` 的挂载报
NTSTATUS `0x80070050`（ERROR_FILE_EXISTS）。重启即可清除；CLI 已把该错误翻译成可操作提示。

## 7. 限制

* **只读**：所有写操作返回只读错误。
* 目录列举会一次性拉完该目录的所有分页（每页 1000 条），超大目录首次进入会慢；不预取整棵树。
* 云端在挂载期间被删除/改名/改大小：命中严格校验后会失效缓存并返回错误，
  资源管理器刷新后可恢复；不会静默返回错误数据。
* 直链有效期短，读取失败会自动重取直链（有界重试）；直链过期不重试到底就报错。
* 一个进程内的挂载由 `MountHandle` 的生命周期管理；**不要** `kill -9` 挂载进程，
  否则可能留下一个驱动层的陈旧卷（见故障排查）。
* 网络抖动时界面会显示错误并继续可用；不自动重连到需要重新登录的会话。

---

## 8. 故障排查

| 现象 | 原因 | 处理 |
|------|------|------|
| 挂载报"未找到 WinFsp 运行时" | 没装 WinFsp 或装的是无 `bin\winfsp-x64.dll` 的版本 | 装 <https://winfsp.dev/rel/> 后重试 |
| 挂载报"NTSTATUS 0x80070050"（ERROR_FILE_EXISTS） | 同名 UNC 卷已被**上一次异常退出**的进程占用 | 结束该进程（任务管理器）或重启；或换 `--unc-prefix` |
| 盘符被占用 | 该盘符已有映射 | 换盘符；GUI 会列出空闲盘符 |
| 读取报"服务端不支持 Range 分段读取" | 该文件的 CDN 节点忽略了 Range | 无法用零缓存模式读；界面/CLI 会明确报错而不是偷偷整包下载 |
| 读取报"文件大小已变化" | 云端文件在挂载期间被改过 | 刷新目录后重试 |
| 卸载后盘符还在 | 有进程仍持有该卷的句柄 | 关闭资源管理器窗口/相关程序后重试 |
| Linux 报 `/dev/fuse` 不存在 | 没装 fuse3 或没权限 | 装 fuse3，把用户加入 fuse 组 |

诊断命令：

```text
tianyi-app -c mounts --json                                  # 当前挂载与统计
"C:\Program Files (x86)\WinFsp\bin\fsptool-x64.exe" lsvol    # WinFsp 侧看到的卷
cargo run -p tianyi-mount --example win_abi_probe            # WinFsp ABI 自检（无需账号）
cargo run -p tianyi-mount --example netdrive_probe -- --mount Z: --unc-prefix '\Probe\Personal'
```

---

## 9. 验收记录（真机，Windows 11 + WinFsp 2.1）

以下结果来自本仓库实测，用来区分"跑过"与"想过"：

```text
账号：19051035392@189.cn  空间：personal
准备挂载到 Y:
  [通过] GetDriveTypeW == DRIVE_REMOTE：Y:\ → 4（期望 4）
  [通过] 目录列举：11 项：["BanG Dream相关", "LoRA", "sd-webui-forge-neo-aki-v1.7z", "同步盘", "备份", "我不好说"]
  [通过] 文件读取（走 Range）：Y:\sd-webui-forge-neo-aki-v1.7z 声明 3337205653 字节，实读 4096 字节，前 8 字节 [37, 7a, bc, af, 27, 1c, 00, 04]
  [通过] 创建文件被拒绝：拒绝访问。 (os error 5)
读取统计：累计 116629 字节 / 3 次请求 / 0 次失败；最近速度 38.0 KiB/s；目录缓存 1 个
  [通过] 卸载：已卸载
  [通过] 卸载后盘符释放：Y:\ 卸载后类型 1
全部验收项通过
```

* 3.3 GB 的 7z 文件只读了前 4 KiB，前 8 字节是 `37 7a`（"7z" 魔数），说明读到的是真实的文件头而不是全量下载。
* `fsptool lsvol` 在挂载期间能看到：
  `X:  \Device\Volume{...}\TianyiCloud-19051035392-189-cn\Personal` —— 卷名就是 UNC 前缀。
* **跨进程浏览**：根目录枚举已验证（`cmd /c dir X:\` 与 PowerShell `Get-ChildItem X:\` 都通过：
  11 项、大小与时间正确、卷标与序列号正常）；**子目录递归枚举存在第 4 节记录的已知缺陷**。
* **尚未验证**：Linux FUSE 全部行为（本机没有 Linux 环境）；GUI 页面的真实挂载交互
  （没有桌面会话）；UNC 路径经由网络提供程序的实际 `readdir`（该检查在早期版本上触发过自访问
  死锁，现在改成 `--unc` 显式开启，尚未在修复后复测）。
* 上述未验证项与已知缺陷都在本文件与提交说明里明确列出，不当作已完成。

---

## 10. 开发与测试

```text
cargo test -p tianyi-mount                       # 单测 + 集成测试（含零缓存静态审计）
cargo clippy -p tianyi-mount --all-targets       # 注意：本机若 PATH 里有 rustup 的 MSVC 影子，
                                                 # 要显式用 C:\msys64\ucrt64\bin\cargo-clippy.exe
cargo fmt -p tianyi-mount -- --check
```

测试覆盖面：

* `paths`：非法字符、保留设备名、超长名截断、大小写/清洗后重名去重、`..` 拒绝；
* `reader`（自带 mock HTTP 服务）：精确 Range、尾部截断、越界、零长度不发请求、
  **200 整包拒绝**、Content-Range 起点不符、短读、直链过期刷新、并发上界、预读不额外发请求、重试有界；
* `fs`（内存 mock 后端 + mock Range 服务）：逐段解析、缓存命中与 TTL 回退、直链复用、
  只读语义、容量失败降级、缓存失效；
* `win::sys`：结构体 sizeof/offset 断言、动态加载探测、dirname 布局、宽字符串往返；
* `win`：VolumeParams 位标志与 UNC 前缀（这是"网络驱动器"的判定依据）、长前缀截断、
  FileInfo 属性映射、错误到 NTSTATUS 的映射；
* `tests/windows_env`：GetDriveTypeW 绑定、空闲盘符、系统保留盘符拒绝、WinFsp 可用性提示；
* `tests/no_disk_cache_audit`：零磁盘文件缓存的静态护栏。
