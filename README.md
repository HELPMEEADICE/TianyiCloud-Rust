# 天翼云盘客户端 (TianyiCloud-Rust)

使用 **Rust + Slint** 开发的天翼云盘第三方桌面客户端，API 实现移植自 [OpenList](https://github.com/OpenListTeam/OpenList) 的 `189pc` 驱动。

## 功能特性

### 登录
- **密码登录**：RSA 加密用户名/密码，验证码弹窗支持
- **二维码登录**：内嵌二维码，天翼云盘 App 扫码即登
- **登录态持久化**：token 自动保存，会话自动刷新（`getSessionForPC.action` + `refreshToken.do`），无需重复登录

### 文件管理
- 文件/文件夹列表（分页加载、图标缩略图）
- 面包屑导航 / 上级目录
- 新建文件夹、重命名、删除、复制、移动
- 提取下载链接（302 重定向解析直链）
- 个人云 / 家庭云空间切换（家庭云端点 `family/file` + familyId）
- 容量显示

### 传输
- **上传**：分片并发上传（线程数可配），支持**秒传**（同 MD5 免流量）、断点续传
- **下载**：并发 Range 分片下载，支持**断点续传**（`.part` 临时文件），失败自动重试
- **任务管理器**：任务列表、进度条、暂停/取消、任务状态持久化（`tasks.json`），重启后恢复

### 预览
- 图片：内置大图预览（`image` crate 解码）
- 音频/视频/PDF/Office/文本：下载临时副本后调用系统默认程序打开

### 搜索
- 全局文件搜索（`searchFiles.action`），结果列表可进入

### 增值功能
- **CAS torrent 秒传**：上传时可生成包含 `x-cas` 扩展（`cloud/file_md5/slice_md5/slice_size/slice_md5s`）的 torrent 文件，支持解析 torrent 提取 CAS 信息

## 技术栈

| 组件 | 说明 |
|---|---|
| `slint` | GUI 框架（声明式 UI） |
| `tokio` + `reqwest` | 异步 HTTP 与运行时 |
| `rsa` / `aes` / `hmac` / `md-5` / `sha1` | 天翼云盘签名与加密（AES-ECB、HMAC-SHA1、RSA PKCS1v15） |
| `image` / `qrcode` | 图片解码与二维码渲染 |
| `rfd` | 文件选择对话框 |

## 项目结构

```
crates/
├── tianyi-core/          # 纯 Rust 核心库
│   └── src/
│       ├── api.rs        # 登录（密码/二维码）、会话刷新
│       ├── client.rs     # 签名请求、文件列表/操作、批量任务、下载链接
│       ├── crypto.rs     # 签名/AES/RSA/MD5/分片大小计算
│       ├── transfer.rs   # 上传（秒传/分片/断点续传）、下载（Range 并发）
│       ├── torrent.rs    # CAS torrent 生成与解析（bencode）
│       ├── task.rs       # 传输任务管理器（持久化）
│       ├── models.rs     # API 数据结构
│       ├── config.rs     # 账号/配置持久化（%APPDATA%/tianyi-cloud-rust）
│       └── file.rs       # 文件类型识别与预览辅助
└── tianyi-app/           # Slint 桌面 UI
    ├── ui/main.slint     # 界面定义（登录/文件列表/传输面板）
    └── src/
        ├── controller.rs # UI 事件 ↔ 核心库桥接
        ├── backend.rs    # tokio 运行时调度
        └── main.rs
```

## 构建与运行

```bash
cargo build --release
./target/release/tianyi-app.exe
```

开发模式：`cargo run -p tianyi-app`

## 配置与数据存储

应用数据保存在系统配置目录下的 `tianyi-cloud-rust/`：
- `config.json` — 全局配置（下载目录、并发数等）
- `accounts.json` — 多账号及 token（自动刷新）
- `tasks.json` — 传输任务进度（断点续传恢复）

## API 签名说明

天翼云盘 API 需要以下签名机制（移植自 OpenList）：
1. 每个请求携带 `clientSuffix` 查询参数（`clientType/version/channelId/rand`）
2. 参数经 **AES-ECB**（key = sessionSecret 前 16 字节，PKCS7 填充）加密为 `params`
3. 请求头携带 `Date`（GMT）、`SessionKey`、`X-Request-ID`（UUID）、`Signature`
4. `Signature = UPPER(hex(HMAC_SHA1(sessionSecret, "SessionKey=..&Operate=..&RequestURI=..&Date=..&params=..")))`

## 免责声明

本客户端仅用于个人学习与自用，天翼云盘接口可能随时变更，请合理使用。

## 许可证

GPLv3
