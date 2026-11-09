# 贡献指南

天翼云盘客户端（`TianyiCloud-Rust`）是 Rust + Slint 的桌面项目。这份文档只写**怎么在本仓库里改东西并让它过门禁**，
功能说明见 [README.md](README.md)，历史改造记录见 [docs/](docs/)。

## 1. 环境

| 项 | 要求 |
|---|---|
| Rust | 仓库根 `rust-toolchain.toml` 固定 `channel = "stable"` + `rustfmt` / `clippy` 组件，进入目录即生效 |
| MSRV | 见 `Cargo.toml` 的 `[workspace.package].rust-version`（当前 1.92，依据 slint 1.17.1 自身声明）；CI `msrv` 作业按此校验 |
| Linux GUI 依赖 | `libfontconfig1-dev libfreetype6-dev libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libgl1-mesa-dev libegl1-mesa-dev libx11-dev libxcursor-dev libxrandr-dev libxi-dev libxinerama-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev pkg-config` |
| Windows | MSVC 目标由 CI 覆盖；本机若用 MSYS2 ucrt64，见下面的 clippy 例外 |

## 2. 构建

```bash
cargo build --release                    # release 产物：target/release/tianyi-app.exe
cargo build                              # debug 构建（默认不编译 icon-sheet 开发组件）
cargo run -p tianyi-app                  # 开发模式直接跑
```

## 3. 五条门禁命令

提交前请本地跑完这五条（CI 里前三条 + `cargo check` 是硬门禁）：

```bash
# 1 格式
cargo fmt --all -- --check

# 2 静态检查（CI 用 -D warnings，告警即失败）
cargo clippy --workspace --all-targets --locked -- -D warnings

# 3 单元测试（含 UI 几何断言与核心库集成测试）
cargo test --workspace --locked

# 4 编译校验（三平台矩阵在 CI，本地至少跑一遍本机目标）
cargo check --workspace --all-targets --locked

# 5 UI 截图（几何断言之外的人工复核，产物写进 screenshots/after）
cargo run -p tianyi-app --example ui_preview -- screenshots/after
```

## 4. 本机 MSYS2 环境下 clippy 的替代命令

**在本机（MSYS2 ucrt64、无 rustup、无 `link.exe`）`cargo clippy` 整体不可用**，直接跑会失败。

- **原因**：`cargo-clippy` 会转发到 rustup 的 **msvc** 工具链，而本机没有 `link.exe`；同时本机
  `cargo-clippy.exe` 的版本与 `rustc` 不匹配，编译 build script 时会按 MSVC host 解析链接器而报
  `linker link.exe not found`。
- **替代命令**（与 `cargo clippy --workspace --all-targets -- -D warnings` 等价，只对 workspace 成员生效，不影响依赖）：

  ```powershell
  # 推荐：仓库内已封装的等价入口
  pwsh tools/clippy.ps1

  # 等价的手写形式
  $env:RUSTC_WORKSPACE_WRAPPER = 'C:\msys64\ucrt64\bin\clippy-driver.exe'
  $env:CLIPPY_ARGS = '-Dwarnings'
  cargo check --workspace --all-targets --locked
  ```

  这是**环境限制的绕行**，不是代码问题：CI（Linux runner）上的 `cargo clippy` 正常，判定以 CI 为准。

## 5. 提交规范

- **Conventional Commits**：`<type>(<scope>): <中文简述>`，`type` 取 `feat` / `fix` / `chore` / `ci` / `docs` / `test` / `refactor` / `perf`。
  例：`fix(core): HTTP 客户端构建失败不再 panic`。
- **路径级暂存**：只 `git add <具体文件>`，不要 `git add -A` / `git add .` / `git add -u`，也不要 `git commit -a`。
  多人（多代理）并行改同一个工作区时，路径级暂存是唯一能保证不误提交别人改动的做法。
- **不 push、不改历史**：不 `git push`、不 `--amend`、不 rebase / reset 已提交内容；由维护者统一推送。
- 一个提交只做一件事；**格式、门禁、文档改动不要混进功能提交**。
- 提交前确认：`git status` 里没有你不想提交的路径。

## 6. `icon-sheet` feature（图标总览）

图标全览界面是**开发自发检**用的，默认构建不编译：

```bash
# 生成图标总览（需要显式打开 feature）
cargo build --features icon-sheet --example icon_sheet
cargo run -p tianyi-app --features icon-sheet --example icon_sheet -- screenshots/icon-sheet
```

- 只有带 `--features icon-sheet` 时才会编译 `crates/tianyi-app/ui/dev/icon-sheet.slint`；
  常规 `cargo build` / CI 的 clippy、test、check 都不包含它。
- 新增或替换图标时：把 Fluent SVG 放进 `crates/tianyi-app/ui/assets/icons/`，在
  `ui/theme/icons.slint` 的 `IconName` 注册，并同步同目录 `SOURCES.md` 的来源记录。

## 7. 发布与打包产物

- `ci.yml`：fmt / clippy / 三平台 check / test / test-windows / msrv + 版本一致性校验（`tools/check-versions.ps1`）。
  `audit`（cargo-deny）与 `coverage`（cargo-llvm-cov）是**非阻塞**作业，只出报告。
- `build.yml`：Release 触发时构建 6 个目标；Windows 侧产物为 `.msi` 安装包、`TianyiCloud-<version>-<arch>.zip`
  以及免安装的 `tianyi-cloud-<version>-<arch>-portable.zip`（exe + LICENSE + README）。
- 改版本号时：只改 `Cargo.toml` 的 `[workspace.package].version`，然后跑 `pwsh tools/check-versions.ps1`
  确认打包模板没有残留硬编码版本号。
