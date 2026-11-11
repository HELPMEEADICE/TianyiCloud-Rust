# 天翼云盘客户端 (TianyiCloud-Rust)

使用 **Rust + Slint** 开发的天翼云盘第三方桌面客户端，界面按 Windows 11 / Fluent Design 2 的视觉与交互规范重做。
API 实现移植自 [OpenList](https://github.com/OpenListTeam/OpenList) 的 `189pc` 驱动。

## 功能特性

### 登录
- **密码登录**：RSA 加密用户名/密码，验证码内联展示
- **二维码登录**：内嵌真实二维码，完整状态机（获取中 / 等待扫码 / 已扫码待确认 / 已过期 / 失败）
- **登录态持久化**：token 自动保存，会话自动刷新（`getSessionForPC.action` + `refreshToken.do`），启动时自动恢复
- **已保存账号快捷登录**：登录页直接点击账号即可继续登录
- 切换登录方式、刷新二维码、退出登录都会作废在途的异步登录结果（generation 校验），不会出现“旧二维码结果覆盖新会话”

### 文件管理
- Windows 11 资源管理器风格 Details View：名称 / 大小 / 修改时间 / 类型四列，`ListView` 虚拟化
- 真正的面包屑导航（点击任意层级跳转，与目录 ID 状态严格同步）
- 新建文件夹、重命名（F2 原地编辑）、批量删除（含确认框）、复制、拖动移动、移动到上级
- 提取下载直链（302 重定向解析），结果进入“下载直链”页可再次复制
- 个人云 / 家庭云空间切换（家庭云端点 `family/file` + familyId，失败自动回退并提示）
- 容量显示、搜索（输入去抖 + 乱序结果丢弃 + 一键退出搜索）

### 传输
- **上传**：分片并发上传（线程数可配），支持**秒传**（同 MD5 免流量）、断点续传
- **下载**：按 4 MiB 定长分片 + 有界并发下载；`.part` 始终是目标文件的**合法前缀**（分片按偏移升序整片追加），断点续传不会产生空洞
- **暂停 / 继续 / 取消**：协作式检查点实现，暂停时任务在检查点挂起（保留 HTTP 会话与 multipart 会话），继续时原地恢复，不是“假暂停”，也不等同于取消
- **任务状态机**：Pending / Running / Paused / Canceled / Completed / Failed，非法转换被拒绝；进程重启后 `Running` 自动降级为 `Paused`（不假装仍在传输）
- **任务持久化**：`tasks.json` 与旧版本格式双向兼容，历史任务在传输页可见
- **全局速度限制**：设置页「传输 → 速度限制」五档（不限速 / 1 / 5 / 10 / 20 MB/s），写入 `config.json` 的 `speed_limit`（**字节/秒**，0 = 不限）。上传与下载共用**一个**令牌桶（不是每个任务一个，否则设 1 MB/s、两路并发实际能跑 2 MB/s）。只对**新启动**的任务生效，在跑的任务沿用启动时那个桶；手改成非预设值（例如 3 MB/s）时五段一个都不亮、行内小字写出真实速率，不会显示与实际不符的选中项
- **拖拽上传**：从资源管理器把文件/文件夹拖入窗口，自动上传到当前目录；文件夹递归建目录；批次去重不会重复入队
- **网盘内拖拽移动**：按住拖拽文件到目标文件夹行松开
- **拖出下载**（Windows）：Shift/Alt + 拖动文件行，先把云端文件取到临时目录再交给系统拖放

### 预览
- 音频 / 视频 / PDF / Office / 文本：取直链后交给系统默认程序（音视频流式播放，不落盘）
- 图片与其它类型：下载临时副本后打开

### 界面
- WinUI 3 / Fluent Design 2 视觉：中性色分层表面、单一强调色、4px 圆角体系、4px 间距栅格
- 浅色 / 深色 / 跟随系统三态主题，跟随系统时轮询系统偏好并实时切换
- 可折叠 NavigationView（窗口 < **1148px** 自动收窄为 56px 图标栏，折叠态有 Tooltip；断点是算出来的而不是调出来的：`Theme.nav-width` 248 + `LayoutMetrics.min-capability-width` 900 = 1148，只有「展开的导航栏」挤完还剩 900px 内容区才值得展开。批量栏的图标档判据同源，按**内容区** < 900px 触发，所以窗口 900–955px 走图标档——窗口更宽时能力不会反而更少）
- 云盘页工作区限宽 `Theme.workspace-max-width`（1360px）并居中：1920 宽窗口下文件行不再被拉成一条线。上限必须 ≥ `LayoutMetrics.detail-level-of` 的最高档门槛 1300px，否则「类型」列永远不可达（`detail-level` 按限宽后的宽度判定）
- 排序入口与窗口宽度无关：命令栏「排序」下拉（名称 / 大小 / 修改时间，右侧显示当前列的方向箭头），窄窗口下也能按修改时间排序；宽档点击列头排序照旧
- 浮层生命周期统一：Tooltip 在底部目标处翻到目标上方并钳在窗口内、整表换模型（刷新 / 重排 / 换筛选）时熄灭；账号菜单支持 Esc 关闭与 ↑↓ + Enter 触发；窗口失焦时 Tooltip / 账号菜单 / 右键菜单 / 排序浮层一起收掉（它们都算 `modals-blocking`，挂着不收会让背景控件一直退出 Tab 序列）
- 对话框、账号菜单、右键菜单、Toast、Tooltip、命令栏排序下拉六处浮层统一带 `Theme.shadow` 投影（blur 16px / offset-y 4px）；投影的回归断言是**规则式**的（凡用 `Theme.flyout-bg` 作底色的浮层都必须带投影），新增浮层漏配会直接红灯，不依赖"记住把新浮层加进断言名单"
- 可展开的传输抽屉（默认收起，状态栏有实时任务计数）、统一 InfoBar 状态反馈、轻量 Tooltip 浮层
- 零自绘图标：全部图标来自微软官方 Fluent UI System Icons（MIT）的原始 SVG，
  编译期嵌入、以 alpha 蒙版随主题着色，任意 DPI 清晰；默认 Regular、选中态 Filled。
  详见 [docs/ICON_SYSTEM_MIGRATION.md](docs/ICON_SYSTEM_MIGRATION.md) 与
  `crates/tianyi-app/ui/assets/icons/SOURCES.md`
- 动效统一走 `Theme.motion-*` 令牌（90 / 180 / 240ms）：浮层入场、页面切换淡入、传输抽屉展开、
  状态切换淡入与控件状态过渡；「减少动画」开关会把全部动效时长归零。
  配方与取舍见 [docs/MOTION_REPORT.md](docs/MOTION_REPORT.md)
- 支持“减少动画”，关闭全部过渡动画
- 快捷键：F2 重命名、Delete 删除、F5 刷新、Ctrl+A 全选、Ctrl+F 搜索、Alt+← 返回上级（均服从焦点，输入框内不触发）
- **四语自适应**：简体中文 / 繁體中文 / English / 日本語；首次启动按系统语言自动选择（其它语言回退英文），设置页「外观 → 语言」可即时切换、无需重启，选择写入 `config.json`。两套词表的分工、切语链路与新增文案/语种的步骤见 [docs/I18N.md](docs/I18N.md)

## 技术栈

| 组件 | 说明 |
|---|---|
| `slint` | GUI 框架（声明式 UI，纯 Rust 渲染，无 WebView） |
| `tokio` + `reqwest` | 异步 HTTP 与运行时 |
| `rsa` / `aes` / `hmac` / `md-5` / `sha1` | 天翼云盘签名与加密（AES-ECB、HMAC-SHA1、RSA PKCS1v15） |
| `image` / `qrcode` | 图片解码与二维码渲染 |
| `rfd` | 文件选择 / 确认对话框 |

## 项目结构

```
crates/
├── tianyi-core/               # 纯 Rust 核心库（无 UI 依赖）
│   └── src/
│       ├── api.rs             # 登录（密码/二维码）、会话刷新
│       ├── client.rs          # 签名请求、文件列表/操作、批量任务、下载链接
│       ├── crypto.rs          # 签名/AES/RSA/MD5/分片大小计算
│       ├── transfer.rs        # 上传（秒传/分片）、下载（定长分片 + 协作式检查点）
│       ├── task.rs            # 任务状态机、TransferControl（暂停/继续/取消）、持久化
│       ├── torrent.rs         # CAS torrent 生成与解析（bencode）
│       ├── theme.rs           # 系统深色偏好探测（Windows/macOS/Linux）
│       ├── models.rs          # API 数据结构
│       ├── config.rs          # 账号/配置持久化（%APPDATA%/tianyi-cloud-rust）
│       └── file.rs            # 文件类型识别、容量/时间/进度格式化
└── tianyi-app/                # Slint 桌面 UI
    ├── examples/              # ui_preview（界面截图）、icon_sheet（图标总览）
    ├── ui/
    │   ├── main.slint         # 主窗口：路由、导航、抽屉、右键菜单、Tooltip 浮层
    │   ├── types.slint        # 共享数据结构与全局状态（DragOutApi / TooltipState）
    │   ├── theme/
    │   │   ├── tokens.slint   # 设计令牌（颜色/圆角/间距/字号/图标尺寸/动效）
    │   │   └── icons.slint    # 图标注册表（IconName 枚举 + IconAssets + Icon 组件）
    │   ├── assets/icons/      # 微软官方 Fluent SVG（112 个）+ LICENSE + SOURCES.md
    │   ├── dev/               # icon-sheet.slint：图标总览（自检用，不进主界面）
    │   ├── components/        # 按钮、输入框、进度条、导航项、文件行、传输行、InfoBar、空状态…
    │   └── pages/             # login / cloud / transfers / links / settings
    └── src/
        ├── controller.rs      # UI 事件 ↔ 核心库桥接（含暂停/继续链路、请求乱序丢弃）
        ├── backend.rs         # tokio 运行时调度
        ├── dragout.rs         # Windows 拖出下载（COM DoDragDrop）
        └── main.rs            # 事件循环、拖入钩子、UI 定时器
```

## 构建与运行

```bash
cargo build --release
./target/release/tianyi-app.exe
```

开发模式：`cargo run -p tianyi-app`

仓库根目录的 `rust-toolchain.toml` 固定了 `channel = "stable"`，并声明 `rustfmt` / `clippy` 两个组件
（MSRV 在 `Cargo.toml` 的 `rust-version` 里单独声明，CI 的 `msrv` 作业按它校验）；
`cargo` 在该目录下会自动使用这份工具链清单，无需手动 `rustup default`。

## 测试与门禁

- **单元测试**：`cargo test --workspace`，当前 **175 条**（app 90 / i18n 4 / core 73 / persistence 8；
  后两组是集成测试：`crates/tianyi-app/tests/i18n_catalog.rs` 与 `crates/tianyi-core/tests/persistence.rs`）。
  其中 UI 层有
  **22 个 `#[test]`** 基于 Slint 测试后端（`crates/tianyi-app/src/ui_tests.rs`），在无窗口环境下校验布局几何、
  控件可访问语义与键盘交互：1 个聚合的几何断言 + 21 条独立断言，失败时测试名本身就是结论。
  第四轮新增 13 条，守的是断点单调性、排序可达性、断点常量单点化、Tooltip 翻转/换模型熄灯、
  账号菜单键盘闭合、失焦收浮层、工作区限宽、`detail-level` 吃限宽宽度、浮层投影、速度限制行、
  设置页卡片高度与末行不裁切。
  测试后端用线程局部哨兵而非进程级初始化，多个 `#[test]` 因此可以并行。
- **视觉基线**：`screenshots/after/` 共 **29 张**，由 `examples/ui_preview.rs` 采集（`pwsh tools/ui-preview.ps1`
  一条命令重采，默认写到该目录）。比对用 `pwsh tools/visual-diff.ps1 -Before <dir> -After <dir> [-Noise 60]`，
  或 `-BeforeRev HEAD` 直接与上一个提交里的基线比；逐通道统计差异像素数并给包围盒，
  存在 changed 时退出码 1。默认噪声底 60 像素：只有 `06-cloud-loading` 与 `21-login-qr` 的 spinner
  相位会抖（同一二进制两次渲染实测 48 / 26 像素），其余 27 张逐字节相同。
- **代码门禁**：`cargo fmt --all -- --check` 与 `cargo clippy --workspace --all-targets --locked -- -D warnings`
  在 CI 中都是硬门禁（`ci` 汇总作业会因此失败）；`cargo check --workspace --all-targets` 覆盖三平台。
- **本机（MSYS2 ucrt64）跑 clippy**：本机没有 rustup/`link.exe`，`cargo clippy` 会转发到 msvc 工具链而失败，
  用等价替代 `pwsh tools/clippy.ps1`（本质是 `RUSTC_WORKSPACE_WRAPPER=clippy-driver` + `cargo check`），
  详见 [CONTRIBUTING.md](CONTRIBUTING.md)。
- **只出报告、不拦门禁**的两个 CI 作业：`audit`（`cargo-deny` 依赖漏洞/许可证）与 `coverage`
  （`cargo-llvm-cov` 行覆盖率摘要）。它们是 `continue-on-error` 且不在 `ci` 汇总作业的 `needs` 里。

## CI / 发布流水线

| 工作流 | 触发 | 内容 |
|---|---|---|
| `ci.yml` | push 到 `main`、PR、手动 | `cargo fmt --check`、`cargo clippy -D warnings`、三平台 `cargo check`、`cargo test` |
| `build.yml` | 发布 Release、手动触发 | 三平台 × 双架构打包，产物见下表 |

| 平台 | 目标三元组 | 产物 |
|---|---|---|
| Windows x86-64 / arm64 | `x86_64-pc-windows-msvc` / `aarch64-pc-windows-msvc` | `.zip`（便携版）、`.msi`（安装包） |
| macOS x86-64 / arm64 | `x86_64-apple-darwin` / `aarch64-apple-darwin` | `.dmg` |
| Linux x86-64 / arm64 | `x86_64-unknown-linux-gnu` / `aarch64-unknown-linux-gnu` | `.AppImage` |

- 手动触发（Actions → Build → Run workflow）时，每个产物都是独立 artifact，可单独下载；也可填写 `version` 覆盖版本号。
- 发布 Release 时，产物会自动附加到该 Release 的 assets。
- 构建期由 `packaging/ci/apply-ci-profile.py` 把体积优先的 release profile（`opt-level="z"` + `lto="fat"` + `codegen-units=1` + `panic="abort"` + `strip="debuginfo"`）写进工作区 `Cargo.toml`；仓库里提交的 `Cargo.toml` 保持原样，本地构建仍是 `opt-level = 2`。
- arm64 构建使用 GitHub 官方 arm64 hosted runner（`windows-11-arm` / `ubuntu-22.04-arm`），这两个 label 只对公开仓库开放。

## 配置与数据存储

应用数据保存在系统配置目录下的 `tianyi-cloud-rust/`：
- `config.json` — 全局配置（下载目录、并发数、主题、减少动画、界面语言、速度限制 `speed_limit`（字节/秒，0 = 不限））
- `accounts.json` — 多账号及 token（自动刷新；**token 明文落盘**，见「已知限制」）
- `tasks.json` — 传输任务与进度（断点续传恢复；与旧版本格式兼容；写入去抖，关窗时强制 flush）

配置文件损坏不会静默丢数据：`config.json` / `accounts.json` / `tasks.json` 解析失败时改名留档为
`<原名>.corrupt-<时间戳>`（同秒多次则 `-1`…`-9`，再撞则覆盖 `-9`），再从默认值起步。

## API 签名说明

天翼云盘 API 需要以下签名机制（移植自 OpenList）：
1. 每个请求携带 `clientSuffix` 查询参数（`clientType/version/channelId/rand`）
2. 参数经 **AES-ECB**（key = sessionSecret 前 16 字节，PKCS7 填充）加密为 `params`
3. 请求头携带 `Date`（GMT）、`SessionKey`、`X-Request-ID`（UUID）、`Signature`
4. `Signature = UPPER(hex(HMAC_SHA1(sessionSecret, "SessionKey=..&Operate=..&RequestURI=..&Date=..&params=..")))`

## 已知限制

- **分享链接**：客户端只实现了“解析文件下载直链”，没有实现分享链接的创建/管理接口，因此导航项与页面命名为“下载直链”，不展示与能力不符的功能。
- **多选**：支持单选、Ctrl+单击逐项多选、Shift+单击范围选择与 Ctrl+A 全选（选中集合以控制器为唯一事实来源）。
- **键盘可达性**：按钮、图标按钮、导航项、菜单项、复选框、开关、分段控件均可 Tab 聚焦，Space/Enter 触发，且只在键盘操作时显示焦点环；文件列表支持 ↑↓ / Home / End / PageUp / PageDown 移动选择、Shift 扩选（选中行自动滚入视口），以及 F2/Delete/F5/Alt+←/Ctrl+A/Ctrl+F/Esc；对话框打开时背景控件退出 Tab 序列、Tab 在对话框内循环；右键菜单可 ↑↓ 高亮 + Enter 触发；搜索态可按 Esc 退出。账号菜单可用 Esc 关闭、↑↓ 移动高亮 + Enter 触发，窗口失焦时四个浮层一起收掉。**仍未覆盖**：账号菜单没有 Tab 循环、右键菜单按 Tab 仍可能先跳到文件列表（键盘路径实际是方向键 + Enter）、传输行与直链行内部按钮的 Tab 顺序未验证，且**未做真实读屏软件（NVDA / 讲述人）实测**——a11y 断言读的是 accesskit 暴露的同一层，不等价于实测（见 `docs/ROUND4_REPORT.md` §6）。
- **Tooltip 残留**：归属键已唯一化，并加了「切页」与「四个 `ListView` 的 `viewport-y` 变化」两个清零点；但指针静止、行被销毁且不滚动时仍可能残留（没有信号可监，已权衡不做「指针位置 × 归属矩形」的复杂方案）。
- **设置页超高窗口下方留白**：卡片高度只由内容决定（不再被视口拉伸），代价是 1920×1080 下卡片组下方留一块背景——`pages/settings.slint` 注释里的实测口径是内容盒 1008px、四张卡片合计 873px，余量约 135px。Win11 设置页同样是「列表靠上、不拉伸」，这是修「卡片被纵向拉伸」的必然代价，不是漏排。
- **速度限制只对新任务生效**：在跑的传输持有启动时那个令牌桶，换档不会让在途任务突然变快或变慢；拖出下载（`dnd.rs`）走 `DownloadOptions::default()`，不受速度限制约束。
- **`accounts.json` 里的 token 明文落盘**：未接系统密钥链（Windows DPAPI / macOS Keychain 需要平台分支）。
- **无单实例锁**：同时开两个进程会争同一份 `tasks.json`——写入是原子的且损坏会留档，读不到半截 JSON，但两个进程的任务表会互相覆盖。
- **`cancel_loading` 只丢结果不取消请求**：非搜索态点「取消」会停止渲染并提示「已取消加载」，真实网络请求仍会跑完（本轮不引入请求级取消）。
- **配置降级未实跑**：配置目录不可写时退到 `%TEMP%\tianyi-cloud-fallback`，日志随之同源；但 Windows 上 `dirs::config_dir()` 走 `SHGetKnownFolderPath`、不读 `APPDATA` 环境变量，无法强制触发该分支，目前只有单测 + 代码链覆盖。
- **Mica / Acrylic**：未引入 Windows App SDK，窗口背景为纯色（Mica 需平台 API 且跨平台不可用），保留系统原生标题栏以保证拖动/贴边/双击最大化/DPI 缩放在所有场景可靠。
- **拖出下载**仅 Windows 实现；Linux/macOS 上会在状态栏提示不支持。
- 极端情况下（进程被强制结束）`.part` 文件可能残留，下次下载同文件时会从合法前缀继续。

## 免责声明

本客户端仅用于个人学习与自用，天翼云盘接口可能随时变更，请合理使用。

## 许可证

GPLv3

第三方资源：
- 应用内全部 UI 图标来自 [Microsoft Fluent UI System Icons](https://github.com/microsoft/fluentui-system-icons)，
  许可证 MIT，原文见 `crates/tianyi-app/ui/assets/icons/LICENSE`；
  逐图标来源与上游提交号见同目录 `SOURCES.md`。
