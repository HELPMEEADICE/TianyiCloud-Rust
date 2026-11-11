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
- **界面动效层（对齐 Fluent / Win11）**：浮层入场淡入（对话框、账号菜单、右键菜单、Toast、Tooltip）、页面切换淡入转场、传输抽屉拉帘式展开、加载/空/错误状态淡入，并补齐控件状态过渡（复选框勾选与描边、开关滑块配色、导航项文字、分段控件文字、菜单行 hover）。所有动效统一走 `Theme.motion-fast/normal/slow`（90 / 180 / 240ms），受「减少动画」开关全量归零。
- **本轮动效报告**：`docs/MOTION_REPORT.md`，含两条入场配方、为什么 `init` 里抬值不产生动画（Slint 属性惰性求值的实测结论）、验证记录与已知取舍。
- **日志单日滚动上限**：单日日志超过 **8 MiB** 就滚成 `.log.1`，历史逐级后移、最多保留 `.log.1/.2/.3` 三份；`parse_log_date` 扩展到识别 `.log.N`，滚动分段与主文件同属一天、一起按 7 天保留期清理（否则分段会绕过保留期永久堆积）。
- **可访问性（第三层：键盘可用）**：文件列表支持 ↑↓ / Home / End / PageUp / PageDown 移动选择、Shift 扩选，并用 `selection-index` 驱动 `ListView.viewport-y`，选中行始终可见。
- **文件行暴露 `list-item` 语义**：`FileRow` 补上「名称 + 大小 · 修改时间 · 类型」描述与默认动作，读屏不再把文件列表读成空列表。
- **模态焦点闭包**：新增 `ModalsApi.blocking` 与 `ActivateScope.enabled`，对话框/浮层打开时背景控件退出 Tab 序列，对话框内 Tab 循环不再把焦点漏给背景导航项。
- **右键菜单键盘导航**：↑↓ 移动高亮、Enter 触发，初始 `ctx-highlight: -1`，打开瞬间不产生视觉变化。
- **七条独立 UI 断言**：`crates/tianyi-app/src/ui_tests.rs` 新增 7 个独立 `#[test]`（`check_batch_bar_a11y` / `check_login_input_survives_mode_switch` / `check_toast_clears_status_bar` / `check_dialog_focus_closure` / `check_selection_index_brings_row_into_view` / `check_tooltip_clears_on_page_switch` / `check_arrow_key_moves_selection`），失败时测试名本身就是结论，不必从汇总输出里倒推红的是哪一条。
- **三个视觉场景与 25 张基线**：新增 `23-cloud-narrow-actions`（960×760）、`24-cloud-inline-error`（1180×760）、`25-login-invalid`（900×760），`screenshots/after/` 由 22 张增至 25 张。
- **本轮优化报告**：`docs/ROUND3_REPORT.md`，含缺陷清单、五类验证方式、已知偏差与残余限制。
- **四语国际化（简中 / 繁中 / 英 / 日）**：界面文案与运行时文案各一套词表——`.slint` 的 `@tr` 走编译期打包的 `lang/<lang>/LC_MESSAGES/tianyi-app.po`（msgid = 简中原文，简中即翻译表索引 0，故无 `zh-Hans` 目录），控制器 `format!` 拼出来的状态栏/提示条/任务状态走 `i18n/rs/<lang>.json`（键 → 简中原文，缺词按「当前语种 → 简中 → 键名」回退）。设置页「外观 → 语言」即时切换、无需重启，选择写入 `config.json` 的 `language` 字段；「自动」模式按系统 locale 解析（繁体优先，其它语言回退英文）。详见 `docs/I18N.md`。
- **i18n 目录完整性回归测试**：新增 `crates/tianyi-app/tests/i18n_catalog.rs`，扫描 `ui/**/*.slint` 的 `@tr` 与三份 `.po`、四份运行时词表做互校（全覆盖 / 双向无多余 / 键集与占位符一致 / `lang/` 目录卫生）。漏翻此前只会静默回退成中文，现在直接红灯。
- **文案抽取脚本**：`tools/i18n-extract.ps1` 从 `.slint` 抽 msgid（跳过 `ui/dev/`、正确解 `\"`），`-Count` 输出总数/去重数，`-Skeleton <lang>` 生成 `.po` 骨架。
- **全局速度限制**：设置页「传输 → 速度限制」五档（不限速 / 1 / 5 / 10 / 20 MB/s），写入 `config.json` 的 `speed_limit`（字节/秒，0 = 不限）。上传与下载共用**一个**令牌桶（两路并发时总量仍受限于该值），只对**新启动**的任务生效——在跑的任务持有启动时那个桶。手改成非预设值（如 3 MB/s）时五段一个都不亮、行内小字写出真实速率，不会显示与实际不符的选中项。
- **命令栏排序入口**：新增「排序」下拉（`ui/components/sort-menu.slint`），名称 / 大小 / 修改时间三项，右侧显示当前列的方向箭头。此前 `sort-changed` 只有三个列头调用点，而「修改时间」列头在 `detail-level >= 2` 里——默认 1180px 窗口（内容区 932px）根本没有按修改时间排序的入口。
- **浮层投影**：对话框卡片、账号菜单、右键菜单、Toast、Tooltip 五处统一声明 `drop-shadow-*`，几何全部引 `Theme.shadow` / `shadow-blur`（16px）/ `shadow-offset-y`（4px）。`Theme.shadow` 此前零引用，五处浮层在浅色遮罩上是一块没有层次的纯白。**第六处（命令栏排序下拉）在 W6 收口时补上**：它一直漏着，而当时的断言是「五处逐个查」的枚举式，枚举之外一个都抓不到——补投影的同时把断言升级成规则式（凡引用 `Theme.flyout-bg` 的 `.slint` 文件，`Theme.shadow-blur` 出现次数不得少于它，且全仓差额恒为 1），新增浮层漏配会直接红灯。
- **视觉回归工具**：`tools/visual-diff.ps1` 逐通道比对两组基线（输出每张图的差异像素数、差异包围盒与 identical/noise/changed 判定，`-BeforeRev <ref>` 直接与历史版本里的基线比，存在 changed 时退出码 1）；`tools/ui-preview.ps1` 一条命令重采 29 张基线并在结束时给出比对命令。两者零第三方依赖，只用 `System.Drawing` + `LockBits`，不再依赖「本机恰好装了 numpy / Pillow」。

### Changed

- **预览采图口径**：`examples/ui_preview.rs` 不再"固定 sleep 260ms 后推进一次"，改为"先真绘一帧 → 循环（等一帧 + 推进定时器 + 绘制）直到没有活跃动画"，`animation-tick()` 驱动的 spinner/进度条场景用 1s 墙钟上限兜底。旧写法会把"下一帧才触发"的入场动效卡在起始帧，采出半透明/空白图。

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
- **焦点 Scope 样板抽成 `ActivateScope`**：6 个控件里逐字同构的 FocusScope 样板收敛到 `components/activate-scope.slint`，`focus-on-click: false` 由 9 处降到 4 处（1 处唯一实现 + 3 处语义不同的例外，例外就地注明不套用的理由），避免以后任一处被改回默认值就静默复发「单击要点第二次」。
- **删除 `FluentInfoBar` 死组件**：`components/info-bar.slint`（75 行）零引用，直接删除；`pages/cloud.slint` 的两处提示条因此仍是手写实现，取舍见 `docs/ROUND3_REPORT.md` §5。
- **颜色字面量收敛为令牌**：`ui/` 下 8 处颜色字面量收敛为 7 个新令牌（`segmented-track`、`switch-knob`、`qr-surface`、`state-icon-bg`、`state-icon-bg-busy`、`drag-tint`、`flyout-scrim`），取值逐字符相同、渲染零变化；组件层颜色字面量归 **0**。
- **导航折叠断点单点化**：断点收敛到 `ui/types.slint` 的 `LayoutMetrics.nav-compact-of`，README 与代码统一为 **960px**（此前 README 写 1040px，与代码不符）。
- **UI 测试后端改为线程局部哨兵**：从进程级 `init_integration_test_with_mock_time()`（含 proxy，第二个测试线程必然 `AlreadySet` panic）改为线程局部哨兵 + `init_no_event_loop()`，多个 `#[test]` 才可能真正并行。
- **`config.json` 结构版本升到 v2**：新增 `language` 字段（默认 `auto`），缺 `version` 的历史配置按迁移链逐级升级到 v2，字段兼容由 `#[serde(default)]` 兜底，**不需要手动删配置**；读到比当前更高的版本号只记 warn 并按现有结构尽力解析。
- **导航断点从 960px 改为 1148px**，而且不再是拍出来的数字：`Theme.nav-width`（248）+ `LayoutMetrics.min-capability-width`（900）。批量栏图标档的判据与它同源（内容区 < 900px），于是 `(导航档, detail-level, 批量栏档)` 关于整窗宽度单调不减——修之前整窗 960px 时导航展开吃掉 248px，内容区从 903px 掉到 712px，窗口比 959px 宽、能力反而更少。批量栏图标档的可达区间随之由 960–1007px 改为 **900–955px**。
- **测试侧的断点常量单点化**：`ui_tests.rs` 不再自带 `NAV_WIDTH` / `NAV_WIDTH_COMPACT` / `NAV_COMPACT_BREAKPOINT` 副本，内容区宽度由 `Theme` 与 `LayoutMetrics` 推算，并与渲染出来的导航栏实测宽度互校——副本最坏的后果不是报错，而是 UI 改了断点后断言跑在错坐标上照样通过。
- **云盘页工作区限宽 1360px 并居中**（`Theme.workspace-max-width`）：1920 宽窗口的内容区 1672px 不再把文件行拉成一条线。`detail-level` 改为吃**限宽之后**的宽度，因此上限必须 ≥ `detail-level-of` 的最高档门槛 1300px，否则「类型」列永远不可达。
- **设置页卡片高度只由内容决定**：内容列的 `HorizontalLayout` 交叉轴对齐改为 `start`，纵向余量全部还给 `ScrollView`。此前 1920×1080 下每张卡片被均摊 +33.8px（卡片内部大块空白），反方向则把卡片压扁。
- **进度落盘改为去抖**：`set_progress` 不再每次都在 worker 线程上整表 `fsync`（1 秒窗口内最多写一次），关窗时由 `flush_progress` 强制补写最后一段。中断最坏丢一个窗口的进度，换来高频进度回调不再反复重写整份 `tasks.json`。
- **原子写的临时文件名唯一化**：`<name>.tmp` 改为 `<name>.tmp-<pid>-<序号>`，两个线程同时写同一路径不再互相覆盖；留档名同秒多次则 `-1`…`-9`，再撞复用 `-9`（不无限加后缀）。
- **`config.json` / `accounts.json` 损坏不再静默**：解析失败时改名留档为 `<原名>.corrupt-<时间戳>`，再从默认值起步，坏文件留在原地可捞回。
- **删除死字段 `AccountConfig.password`**：从未被读取，却写进每一份 `accounts.json`；旧文件里带这个字段仍能正常加载。

### Fixed

- **CI 全线红灯（三处独立原因）**：① `controller::dnd` 里 `handle_drag_out` 的非 Windows 分支是私有方法，父模块 `controller` 调不到（`E0624`），Linux/macOS 直接编译失败；同时 7 个只在 `cfg(windows)` 函数体里出现的 `use` 没做平台门控，非 Windows 目标逐个报 `unused_imports`（clippy 作业 `-D warnings` 全红）。② Windows 的 build script 在 MSVC 工具链下被 `STATUS_STACK_OVERFLOW` 打死——Windows 主线程栈默认 1MB，而 slint 编译器处理整棵组件树加 `.po` 打包是深度递归的，改为把编译放进 64MB 自建栈的线程里跑，栈预算不再取决于链接器写的 PE 头。③ `cargo-deny` 的许可证白名单缺 `MPL-2.0` / `NCSA` / `CDLA-Permissive-2.0` / `LicenseRef-Slint-Royalty-free-2.0`，两个工作区 crate 也没有 `license` 字段被判 `unlicensed`。
- **依赖告警清理（audit 作业转绿）**：升级 rustls 0.23.46 / rustls-webpki 0.103.16 / hyper-rustls 0.27.10 / webpki-roots 1.0.11 / hyper 1.12.0 / tokio 1.54.1 / pulldown-cmark 0.13.6 / clipboard-win 5.4.3 / async-executor 1.14.1，消掉 9 条 advisory 与 2 条 yanked 告警；剩下 8 条（`rsa` 的 Marvin 攻击 + GUI 栈上 7 个 unmaintained 传递依赖）升级消不掉，在 `deny.toml` 里逐条写明理由后 ignore，上游修好会以 `advisory-not-detected` 提醒删除。

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
- **三份持久化文件改为原子写**：`tasks.json` / `config.json` / `accounts.json` 统一走 `tianyi-core/src/fsutil.rs::write_atomic`（同目录临时文件 → `fsync` → `rename`），写到一半中断不会留下半截 JSON 把整份数据读坏。
- **`tasks.json` 解析失败不再丢整份历史**：损坏时改名留档为 `tasks.json.corrupt-<ts>`，而不是静默丢弃后从空列表重新开始。
- **`tianyi-core` 锁中毒不再级联 panic**：core 侧 9 处 `.lock().unwrap()` 换成 `lock_or_recover`（`tianyi-core/src/lib.rs`），持锁 panic 后仍可继续使用内部数据；core 侧 `.lock().unwrap()` 归零（上一轮只治理了 `tianyi-app` 侧 97 处）。
- **关窗竞态不再覆盖「已暂停」**：新增 `shutting_down` 标志与 `close_safe_status()`，进程退出过程中在跑任务只被归一为 `Paused`，不会被几毫秒后到达的 `Canceled/Completed/Failed` 覆盖写回。
- **日志目录与配置目录同源**：`logging::init(root)` 接受 `ConfigStore::open().root()`，配置降级到 `%TEMP%` 时日志随之同源，设置页两行路径不再互相矛盾。
- **窄窗口批量操作按钮有可读名称**：`narrow` 分支补 `tooltip` 与 `accessible-label` 回退链（text → tooltip → 「按钮」）；实测该分支在窗口 **960–1007px** 区间可达，此前这一档的批量按钮既无文字也无提示。
- **登录方式切换不再丢失输入**：账号与密码提升为页面级 `user-text` / `pass-text`，切「密码 / 扫码」不再清空已输入内容。
- **Toast 不再压住传输胶囊**：Toast 上移 `statusbar-height + space-2`，可点的传输胶囊不会被浮层盖住。
- **点击当前空间的导航项不再白跑一趟**：已在目标空间根目录且非搜索态时，不再清空路径栈与选择集、也不再发起多余请求。
- **Tooltip 同类文案不再互相关灯**：归属键加入绝对坐标，两个同文案控件不再因阻塞同一个键而互相收尾。
- **切页与滚动时收起 Tooltip**：切页以及四个 `ListView` 的 `viewport-y` 变化时清空气泡，避免行被回收后浮层残留在原位。
- **`FluentTextField.invalid` 接上登录失败态**：此前属性存在却无人赋值，登录失败只有 Toast，输入框本身没有视觉反馈。
- **加载态接上「取消」入口**：`cancel_loading` 打通，加载中可以退出，不再只能等请求自己返回。
- **搜索态补 Esc 出口**：搜索中按 Esc 可退出；搜索中点击「取消」委托 `clear_search()`，不再落到误导性的「没有找到匹配的文件」。
- **状态栏胶囊等回归断言的假绿修正**：见 `docs/ROUND3_REPORT.md` §5，`b181cd7` 提交信息里「22 张逐像素相同」的结论已由 numpy 重算推翻（实为 20 张相同 + 2 张 spinner 相位噪声）。
- **窗口更宽、能力反而更窄（D-11）**：导航折叠按「整窗 < 960px」、批量栏退化按「内容区 < 760px」两条不同源的判据各自为政，960–1007px 区间里导航展开、批量栏掉档，工具栏标签与状态栏容量文字一起消失。两条判据现在都由 `LayoutMetrics.min-capability-width` 推导，图标档区间变为 900–955px。
- **窄窗口无法按修改时间排序（D-12）**：排序入口只挂在列头上，「修改时间」列头在窄档根本不在元素树里。改由命令栏下拉提供与窗口宽度无关的入口，浮层开着时计入 `modals-blocking`。
- **Tooltip 只有单边钳制（D-14）**：底部目标的 `y` 只有 `Math.min(窗口高 − 自身高 − 4px)` 一条钳位，气泡被顶在窗口下缘、压住目标本身。改为放不下就翻到目标上方，并补 `Math.max(4px, …)` 下界。
- **整表换模型时 Tooltip 不熄灯（D-15）**：刷新目录 / 重排 / 换筛选 / 搜索结束走的是整份换模型，行实例被销毁、行内按钮收不到 `has-hover = false`，气泡挂在已经不存在的控件上。`changed file-list` 与 `changed active-transfer-list` 各补一个清零点。
- **账号菜单没有键盘层（D-16）**：此前只有一层「点外部关闭」的透明遮罩，Esc 关不掉、↑↓ 不移动，而它与右键菜单同为模态浮层。补 Esc 关闭、↑↓ 移动高亮（跳过不可用项）、Enter 触发。
- **窗口失焦不收藏浮层（D-17）**：`Focused(false)` 之后「指针离开」与「按 Esc」都不会再发生，Tooltip / 账号菜单 / 右键菜单 / 排序浮层原样挂在屏幕上；它们算在 `modals-blocking` 里，背景控件还会一起退出 Tab 序列。失焦时四个浮层一起收掉。
- **云盘页是唯一不限宽的页面（D-19）**：1920 宽窗口的内容区 1672px 把文件行拉成一条线。改为限宽 1360px 并居中，且 `detail-level` 吃限宽后的宽度。
- **`main.rs` 里 5 处 `.lock().unwrap()`**：拖放状态锁中毒会直接 panic 掉事件回调，改为 `lock_or_recover`，与 core / controller 侧口径一致。
- **演示数据自相矛盾（D-25）**：`examples/ui_preview.rs` 写死 `active_transfers = 3`、「进行中的传输」抽屉装的却是全部四条任务（含已完成与失败）、状态栏文案又另写死「1 个任务进行中」，同一批基线图上并排出现三个互相矛盾的数字。改为三处全部由 `demo_transfers()` 推导，口径与控制器一致（active = Pending | Running | Paused，finished = Completed）。

## [0.1.0]

首个可运行版本：密码 / 二维码登录、文件管理（面包屑导航、批量操作、拖拽移动）、
上传（秒传 / 分片 / 断点续传）与下载（定长分片 + 协作式检查点）、传输任务持久化、预览、
三平台打包（`msi` / `dmg` / `AppImage`）。
