# 更新日志

本项目的所有重要变更都记录在这里。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [Unreleased]

### Added

- **请求级重试与指数退避**：核心库的 API 请求支持按状态码/错误类型重试，退避区间指数增长，避免瞬时故障直接失败。
- **配置版本号与迁移链**：`config.json` 带上 `version` 字段与显式迁移链，缺 `version` 的历史配置逐级升级到当前版本，不需要用户手动删配置；读到比当前更高的版本号只记 warn 并按现有结构尽力解析，不崩。（`accounts.json` 本轮未加版本字段。）
- **UI 层测试后端与几何断言**：引入 Slint 测试后端，在无窗口环境下对布局宽度/间距做几何断言，防止改样式时静默错位。
- **核心库持久化公共 API 集成测试**：覆盖持久化的对外接口（而不仅是内部函数），回归时能定位到真实调用路径。
- **CI 增加 Windows 测试与 MSRV 作业**：`test-windows`（windows-2022 上跑 `cargo test`）与 `msrv`（按 `rust-version` 校验下限）。
- **CI 增加 cargo-deny 与覆盖率作业**：`audit` 检查依赖漏洞与许可证、`coverage` 输出行覆盖率摘要；两者 `continue-on-error`，只出报告不拦门禁。
- **Windows 便携包（绿色版）**：新增免安装产物 `tianyi-cloud-<version>-<arch>-portable.zip`（可执行文件 + `LICENSE` + `README.md`），版本号复用构建流水线解析出的同一份版本。
- **版本一致性校验脚本**：`tools/check-versions.ps1` 比对 `Cargo.toml` 的 `[workspace.package].version` 与 Windows/macOS 打包模板，并在 CI 中调用。
- **文档归档**：历史报告（UI/UX 重构报告、图标系统迁移报告）移入 `docs/`，新增 `CHANGELOG.md` 与 `CONTRIBUTING.md`。
- **可访问性（第一层：可识别）**：启用 slint 的 `accessibility` feature，为按钮、图标按钮、导航项、菜单项、文本输入、复选框、开关补 `accessible-role` / `accessible-label` / `accessible-action-default`，导航栏标记为 `navigation` 地标。
- **可访问性（第二层：可操作）**：上述控件可聚焦，Space/Enter 触发动作并显示 2px 焦点环；焦点环只在键盘操作时出现（winit 原始输入事件驱动，`focus-reason` 作为无窗口场景的兜底）。Tab 遍历由 Slint 运行时提供，无需自建焦点模型。
- **日志落盘**：日志同时写入 stderr 与配置目录下的 `logs/tianyi-cloud.YYYY-MM-DD.log`，按本地日期滚动并保留 7 天；写盘失败自动退化为仅 stderr。设置页「关于」分组新增日志目录行（打开 / 复制）。
- **直链页增强**：直链行单击即在默认浏览器中打开，页面顶部新增「复制全部」（按行拼接后一次复制）。
- **本轮优化报告**：`docs/OPTIMIZATION_REPORT.md`，含缺陷清单、验证方式与自我修正记录。

### Changed

- **常规构建不再编译 icon-sheet 开发组件**：图标总览改为 `icon-sheet` feature 门控（`cargo build --features icon-sheet --example icon_sheet`），日常构建和 CI 的编译时间/体积不再包含这块开发界面。
- **引入 `rust-toolchain.toml` 并校准 MSRV 声明**：`rust-version` 由 1.75 修正为 1.92，依据 slint 1.17.1 自身声明的最低版本；此前声明的 1.75 无法成立。
- **release profile 启用 thin LTO 与 strip**：发布构建开启 `lto = "thin"`、`codegen-units = 1` 与 `strip = true`，产物更小且保留可接受的编译耗时。
- **清理既有 clippy 告警（PKCS7 填充）**：PKCS7 填充相关写法按 clippy 建议改写，`cargo clippy -- -D warnings` 保持干净。
- **控制器拆分为模块**：`controller.rs`（4413 行）拆为 `controller/{mod,files,transfers,session,dnd}.rs`，函数名集合与测试集合完全一致，行为零变更。
- **颜色令牌收敛**：Tooltip 浮层与进度条轨道色从硬编码改为 `Theme` 令牌；`ui/` 下的颜色字面量由 11 条降到 7 条，本轮新增代码零新增字面量。
- **性能：目录大小统计移出 UI 线程**：拖入大目录不再冻结窗口，改由 `spawn_blocking` 统计后回填。
- **性能：UI 线程直通**：在 UI 线程上调用时不再多排一次事件循环跳转；非 UI 线程仍走投递。
- **性能：传输列表增量刷新**：改为快照比对后再重建模型（进度变化超过 1% 才推送），切换筛选项强制推送一次。
- **性能：任务级并发上限**：同时进行的传输任务最多 3 个，超出的保持排队状态（与分片级并发是两层控制）。
- **设置页「并发分片数」改为可编辑**：−/数值/+ 步进器，范围 1..=8，写回配置并对新任务生效；界面数字恒等于真正生效的值。

### Fixed

- **单击必须一次到位（导航栏 / 右键菜单 / 按钮 / 分段控件）**：`FocusScope` 的 `focus-on-click` 默认 true，这些控件的 FocusScope 覆盖整个控件且声明在 `TouchArea` 之后（命中测试在前），按下时返回 `EventAccepted` 直接终止派发，`TouchArea` 收不到这次点击——表现为"左侧导航要点两下才切页""右键菜单点不动""按钮第一下没反应"。改为 `focus-on-click: false` + `TouchArea` 按下时显式 `fs.focus()`，点击一次即生效，键盘焦点照旧跟手。
- **双击 / Enter 进入文件夹**：此前每次点击都整表替换 `file-list` 模型，ListView 随之重建所有行实例，Slint 的点击计数因"命中项变了"被清零（`set_click_count(0)`），`double-clicked` 永远凑不齐。现在选中态变化走就地更新（`set_row_data`），只有行集合真的变了（换目录 / 换云空间 / 搜索）才换模型；列表补上 `@keys(Return)`，Enter 打开当前唯一选中项（文件夹进入、文件预览）。
- **重命名时 Enter / Delete / Esc 不再被列表快捷键抢走**：`KeyBinding` 在**捕获阶段**判定，匹配上就吃掉按键——把开关写在 `activated` 的 if 里已经太晚，此前在重命名框里按 Delete 会直接删掉选中文件。条件一律移到 `KeyBinding.enabled`，不适用时按键正常传给输入框（Enter 提交、Delete 删字、Esc 取消重命名）。
- **右键菜单关掉之后快捷键仍然有效**：菜单需要焦点才能收 Esc，关闭时把焦点还给文件列表，F5 / Delete / Enter 不再"关掉菜单就集体失灵"。
- **会话失效判定改为结构化错误码**：不再依赖错误字符串匹配判断登录态过期，改为识别结构化错误码，避免接口文案变化后误判/漏判。
- **HTTP 客户端构建失败不再 panic**：初始化路径上不再 `expect` 崩掉进程，改为三级降级（完整 client → 仅 UA/超时的最小 client → `reqwest::Client::new()`）并记 warn 日志。
- **验证码图片不再被非等比拉伸**：登录页验证码改用 `image-fit: contain`（此前被强行拉满容器，3:1 的图被画成 6.6:1）。
- **重命名支持 Esc 取消**：此前 F2 进入输入态后不按回车就无退出通道（行内点击层被禁用，Esc 无效）。
- **危险确认框按 Enter 走取消**：删除等不可撤销操作不再"连按两下回车即执行"；非危险对话框保持 Enter 确认。
- **右键菜单支持 Esc 关闭**：此前只有"点别处"一条退出路径，键盘用户会困在菜单里。
- **Toast 分级自动消失与重复计数**：info/success 3 秒、warning 5 秒、error 8 秒后自动消失；3 秒内相同文案累加显示「×N」；用 generation 号防止旧计时器关掉新提示；手动关闭会让在途计时器失效。
- **文件列表次要列按内容区宽度分档**：此前用整窗宽度判断，1180 宽窗口下会误显示「修改时间」列并挤压名称列。
- **刷新指示条改为不确定进度**：此前宽度按 `mod()` 线性增长后瞬间归零，视觉上是"拉扯"。
- **移除不可点击行的假 affordance**：传输任务行此前有整行手型光标与悬停高亮但点击无任何行为。
- **Tooltip 增加显示延迟与归属令牌**：450ms 延迟避免掠过密集图标时闪烁；归属令牌修复四个写点互相"关灯"（A 的收尾会把刚进入的 B 的 Tooltip 关掉）。
- **macOS 主题轮询由 3 秒放宽到 30 秒**：此前每 3 秒会 spawn 一个 `defaults read` 子进程。
- **配置目录不可用时降级启动**：不再 `expect` panic，依次退到临时目录、内存态，并用 warning 提示"本次运行不会保存设置"。
- **锁中毒不再连锁 panic**：`tianyi-app` 侧 97 处 `.lock().unwrap()` 改为 `lock_or_recover`，持锁 panic 后仍可继续使用内部数据。
- **关窗时取消传输并落盘**：新增 `close-requested` 钩子，把未完成任务归一化为「已暂停」（不是「已取消」——后者是终态会丧失续传能力），避免下次启动看到幽灵任务。
- **删除死字段 `FileEntry.thumb`**：UI 从不渲染它，却每行每次刷新克隆一次字符串。

## [0.1.0]

首个可运行版本：密码 / 二维码登录、文件管理（面包屑导航、批量操作、拖拽移动）、
上传（秒传 / 分片 / 断点续传）与下载（定长分片 + 协作式检查点）、传输任务持久化、预览、
三平台打包（`msi` / `dmg` / `AppImage`）。
