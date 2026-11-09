# 更新日志

本项目的所有重要变更都记录在这里。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [Unreleased]

### Added

- **请求级重试与指数退避**：核心库的 API 请求支持按状态码/错误类型重试，退避区间指数增长，避免瞬时故障直接失败。
- **配置版本号与迁移链**：`config.json` / `accounts.json` 带上配置版本，旧版本配置可沿迁移链升级，不需要用户手动删配置。
- **UI 层测试后端与几何断言**：引入 Slint 测试后端，在无窗口环境下对布局宽度/间距做几何断言，防止改样式时静默错位。
- **核心库持久化公共 API 集成测试**：覆盖持久化的对外接口（而不仅是内部函数），回归时能定位到真实调用路径。
- **CI 增加 Windows 测试与 MSRV 作业**：`test-windows`（windows-2022 上跑 `cargo test`）与 `msrv`（按 `rust-version` 校验下限）。
- **CI 增加 cargo-deny 与覆盖率作业**：`audit` 检查依赖漏洞与许可证、`coverage` 输出行覆盖率摘要；两者 `continue-on-error`，只出报告不拦门禁。
- **Windows 便携包（绿色版）**：新增免安装产物 `tianyi-cloud-<version>-<arch>-portable.zip`（可执行文件 + `LICENSE` + `README.md`），版本号复用构建流水线解析出的同一份版本。
- **版本一致性校验脚本**：`tools/check-versions.ps1` 比对 `Cargo.toml` 的 `[workspace.package].version` 与 Windows/macOS 打包模板，并在 CI 中调用。
- **文档归档**：历史报告（UI/UX 重构报告、图标系统迁移报告）移入 `docs/`，新增 `CHANGELOG.md` 与 `CONTRIBUTING.md`。

### Changed

- **常规构建不再编译 icon-sheet 开发组件**：图标总览改为 `icon-sheet` feature 门控（`cargo build --features icon-sheet --example icon_sheet`），日常构建和 CI 的编译时间/体积不再包含这块开发界面。
- **引入 `rust-toolchain.toml` 并校准 MSRV 声明**：`rust-version` 由 1.75 修正为 1.92，依据 slint 1.17.1 自身声明的最低版本；此前声明的 1.75 无法成立。
- **release profile 启用 thin LTO 与 strip**：发布构建开启 `lto = "thin"`、`codegen-units = 1` 与 `strip = true`，产物更小且保留可接受的编译耗时。
- **清理既有 clippy 告警（PKCS7 填充）**：PKCS7 填充相关写法按 clippy 建议改写，`cargo clippy -- -D warnings` 保持干净。

### Fixed

- **会话失效判定改为结构化错误码**：不再依赖错误字符串匹配判断登录态过期，改为识别结构化错误码，避免接口文案变化后误判/漏判。
- **HTTP 客户端构建失败不再 panic**：客户端构建失败改为返回错误并向上传递，不再在初始化路径上直接 panic 掉进程。

## [0.1.0]

首个可运行版本：密码 / 二维码登录、文件管理（面包屑导航、批量操作、拖拽移动）、
上传（秒传 / 分片 / 断点续传）与下载（定长分片 + 协作式检查点）、传输任务持久化、预览、
三平台打包（`msi` / `dmg` / `AppImage`）。
