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
- **拖拽上传**：从资源管理器把文件/文件夹拖入窗口，自动上传到当前目录；文件夹递归建目录；批次去重不会重复入队
- **网盘内拖拽移动**：按住拖拽文件到目标文件夹行松开
- **拖出下载**（Windows）：Shift/Alt + 拖动文件行，先把云端文件取到临时目录再交给系统拖放

### 预览
- 音频 / 视频 / PDF / Office / 文本：取直链后交给系统默认程序（音视频流式播放，不落盘）
- 图片与其它类型：下载临时副本后打开

### 界面
- WinUI 3 / Fluent Design 2 视觉：中性色分层表面、单一强调色、4px 圆角体系、4px 间距栅格
- 浅色 / 深色 / 跟随系统三态主题，跟随系统时轮询系统偏好并实时切换
- 可折叠 NavigationView（窗口 < 1040px 自动收窄为图标栏，折叠态有 Tooltip）
- 可展开的传输抽屉（默认收起，状态栏有实时任务计数）、统一 InfoBar 状态反馈、轻量 Tooltip 浮层
- 零图标字体：全部图标为 24×24 矢量路径，随主题着色、任意 DPI 清晰
- 支持“减少动画”，关闭全部过渡动画
- 快捷键：F2 重命名、Delete 删除、F5 刷新、Ctrl+A 全选、Ctrl+F 搜索、Alt+← 返回上级（均服从焦点，输入框内不触发）

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
    ├── ui/
    │   ├── main.slint         # 主窗口：路由、导航、抽屉、右键菜单、Tooltip 浮层
    │   ├── types.slint        # 共享数据结构与全局状态（DragOutApi / TooltipState）
    │   ├── theme/
    │   │   ├── tokens.slint   # 设计令牌（颜色/圆角/间距/字号/动效）
    │   │   └── icons.slint    # 矢量图标字形表 + Icon 组件
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
- `config.json` — 全局配置（下载目录、并发数、主题、减少动画）
- `accounts.json` — 多账号及 token（自动刷新）
- `tasks.json` — 传输任务与进度（断点续传恢复；与旧版本格式兼容）

## API 签名说明

天翼云盘 API 需要以下签名机制（移植自 OpenList）：
1. 每个请求携带 `clientSuffix` 查询参数（`clientType/version/channelId/rand`）
2. 参数经 **AES-ECB**（key = sessionSecret 前 16 字节，PKCS7 填充）加密为 `params`
3. 请求头携带 `Date`（GMT）、`SessionKey`、`X-Request-ID`（UUID）、`Signature`
4. `Signature = UPPER(hex(HMAC_SHA1(sessionSecret, "SessionKey=..&Operate=..&RequestURI=..&Date=..&params=..")))`

## 已知限制

- **分享链接**：客户端只实现了“解析文件下载直链”，没有实现分享链接的创建/管理接口，因此导航项与页面命名为“下载直链”，不展示与能力不符的功能。
- **多选**：支持单选与 Ctrl+A 全选两种模式，不支持 Ctrl+点击任意多选。
- **Mica / Acrylic**：未引入 Windows App SDK，窗口背景为纯色（Mica 需平台 API 且跨平台不可用），保留系统原生标题栏以保证拖动/贴边/双击最大化/DPI 缩放在所有场景可靠。
- **拖出下载**仅 Windows 实现；Linux/macOS 上会在状态栏提示不支持。
- 极端情况下（进程被强制结束）`.part` 文件可能残留，下次下载同文件时会从合法前缀继续。

## 免责声明

本客户端仅用于个人学习与自用，天翼云盘接口可能随时变更，请合理使用。

## 许可证

GPLv3
