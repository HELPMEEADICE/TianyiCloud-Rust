# 第三轮优化报告

> 基线 `78d90d2` → 本轮代码终点 `e955f52`，外加本文件所在的 docs 提交。
> **25 个提交**；`git diff --shortstat 78d90d2..e955f52` = **40 files changed, 2644 insertions(+), 498 deletions(-)**。
> 测试从 **106 条增至 126 条**（app 45→61 / core 58→62 / 集成 3→3）。
> 视觉体系（Fluent 2 令牌、尺寸节奏、组件构成）**未改**；新增颜色字面量 **0**——本轮色彩改动只是把 `ui/` 下 8 处字面量搬进 7 个新令牌，取值逐字符相同，渲染零变化。

---

## 1. 本轮做成了什么

| # | 工作流 | 提交数 | 验证方式 |
| --- | --- | --- | --- |
| 1 | W4 核心与工程健壮性：三份持久化文件原子写、`tasks.json` 损坏留档、core 锁中毒、关窗竞态、日志 8 MiB 滚动、日志目录同源 | 6 | 核心库单测 + 真机冒烟（日志落盘 / `tasks.json` 无 `Running`） |
| 2 | W1 UI 缺陷：窄档批量栏可读名、登录输入跨模式留存、Toast 让开状态栏、重复导航请求、Tooltip 归属键与清零点、颜色字面量收敛、断点单点化 | 8 | 7 条新断言中的 4 条 + 25 张视觉基线逐通道比对 |
| 3 | W3 结构一致性：`ActivateScope` 抽取、删除 `FluentInfoBar` 死组件、`invalid` 与 `cancel_loading` 接线 | 3 | `focus-on-click: false` 9→4 清点 + 行为等价渲染比对 |
| 4 | W2 可访问性第三层：列表方向键选择与视口跟随、`list-item` 语义、模态焦点闭包、右键菜单键盘导航、搜索态 Esc | 5 | 7 条新断言中的 3 条 + 键盘路径实测 |
| 5 | W5 验证与文档：七条断言、三个新视觉场景与 25 张基线重采、变异对照表 | 3 + 本提交 | 四道门禁 + 变异自证 + 隔离实验 |

配套的代码事实（可现查）：`focus-on-click: false` 全 `ui/` 恰 **4 处**（`main.slint`、`activate-scope.slint`、`dialog.slint`、`segmented.slint` 各一）；`tianyi-core` 侧 `.lock().unwrap()` 剩 **0 处**，`lock_or_recover` 在 `client.rs` 有 **10 处**调用点（diff 口径：删掉 9 处 `.lock().unwrap()`、新增 10 处 `lock_or_recover(...)`）；`ui/` 下 `#RRGGBB` 形式字面量全部落在 `theme/tokens.slint`，组件层为 **0**。

---

## 2. 缺陷清单与最终状态

编号沿用本轮任务书与 `ui_tests.rs` 注释里的既有编号（`D-*` 缺陷、`A-*` 可访问性、`S-*` 结构），不另造一套。

### 2.1 W4 核心与工程健壮性

| 缺陷 | 状态 |
| --- | --- |
| `tasks.json` / `config.json` / `accounts.json` 三份持久化文件非原子写，中断即留半截 JSON | 已修：统一走 `tianyi-core/src/fsutil.rs::write_atomic`（同目录临时文件 → `fsync` → `rename`） |
| `tasks.json` 解析失败静默丢弃整份历史 | 已修：改名留档为 `tasks.json.corrupt-<ts>` |
| `tianyi-core` 侧 9 处 `.lock().unwrap()` 会在锁中毒后级联 panic | 已修：全部换 `lock_or_recover`（`tianyi-core/src/lib.rs`） |
| 关窗竞态：几毫秒后到达的 `Canceled/Completed/Failed` 覆盖写回的 `Paused` | 已修：`shutting_down` 标志 + `close_safe_status()` |
| 单日日志无大小上限 | 已修：单日 **8 MiB** 滚动，最多 `.log.1/.2/.3` 三份；`parse_log_date` 扩展到识别 `.log.N`，分段与当天同一条 7 天保留线 |
| 配置降级到 `%TEMP%` 时日志目录不同源 | 已修：`logging::init(root)` 接受 `ConfigStore::open().root()` |

### 2.2 W1 UI 缺陷

| 编号 | 缺陷 | 状态 |
| --- | --- | --- |
| D-01 | 窄窗口批量操作按钮既无文字也无提示，读屏读到无名按钮 | 已修：补 `tooltip` + `accessible-label` 回退链（text → tooltip → 「按钮」）；实测 `narrow` 分支在 **960–1007px** 可达 |
| D-02 | 切换「密码 / 扫码」清空已输入的账号与密码 | 已修：提升为页面级 `user-text` / `pass-text` |
| D-03 | Toast 压住状态栏右下角可点的传输胶囊 | 已修：上移 `statusbar-height + space-2` |
| — | 点击当前空间导航项仍清空路径栈/选择集并发起多余请求 | 已修：已在根目录且非搜索态时直接返回 |
| — | Tooltip 归属键在同类文案下碰撞，四个写点互相关灯 | 已修：归属键加入绝对坐标 |
| D-06 | 切页 / 列表滚动后 Tooltip 气泡残留（行已被回收） | 已修：切页与四个 `ListView` 的 `viewport-y` 变化时清空 |
| — | `ui/` 下 8 处颜色字面量散落在组件层 | 已改：收敛为 7 个新令牌，取值逐字符相同 |
| — | README 的导航断点写 1040px，与代码不符 | 已改：断点单点化到 `LayoutMetrics.nav-compact-of`，文档与代码统一为 **960px** |

### 2.3 W3 结构一致性

| 编号 | 缺陷 | 状态 |
| --- | --- | --- |
| — | FocusScope 样板在 6 个控件里逐字复制，`focus-on-click: false` 的关键注释也复制 6 份 | 已改：抽成 `components/activate-scope.slint`，`focus-on-click: false` 由 9 处降到 4 处（1 处唯一实现 + 3 处语义不同的例外，例外就地注明理由） |
| S-02 | `components/info-bar.slint`（75 行）零引用死代码 | 已删。**未**采用「复用手写提示条」方案，理由见 §5.3 |
| — | `FluentTextField.invalid` 属性存在但无赋值点，登录失败无字段级反馈 | 已修：接上登录失败态 |
| — | 加载态没有退出入口 | 已修：`cancel_loading` 打通（非搜索态只丢弃结果并提示「已取消加载」，不取消真实网络请求，见 §7.9） |

### 2.4 W2 可访问性第三层

| 编号 | 缺陷 | 状态 |
| --- | --- | --- |
| A-01 | 文件列表不能键盘移动选择，选中行滚出视口 | 已修：↑↓ / Home / End / PageUp / PageDown + Shift 扩选，`selection-index` 驱动 `ListView.viewport-y` |
| — | 文件列表对读屏是空列表 | 已修：`FileRow` 暴露 `list-item` 语义（名称 + 「大小 · 修改时间 · 类型」描述 + 默认动作） |
| A-03 | 对话框打开时 Tab 仍能把焦点送到背景控件（实测 Tab×5 + Space 把 `current-page` 切到 2） | 已修：`ModalsApi.blocking` + `ActivateScope.enabled`，对话框内 Tab 循环 |
| — | 右键菜单只能鼠标点 | 已修：↑↓ 高亮 + Enter 触发，初始 `ctx-highlight: -1`（打开瞬间无视觉变化） |
| — | 搜索态没有键盘出口，点「取消」落到误导性的「没有找到匹配的文件」 | 已修：Esc 退出；「取消」委托 `clear_search()` |

---

## 3. 非 UI 改动（摘要）

**正确性**：持久化写入从「直接 `fs::write`」改为原子写，三份文件共用一条路径；`tasks.json` 损坏从「静默丢弃」改为「改名留档 + 从空列表起步」；core 侧锁中毒不再级联 panic；关窗收尾引入 `shutting_down` 让「已暂停」成为退出期唯一终态。

**工程**：日志从「按天滚动 + 7 天保留」补上单日 **8 MiB** 上限与 3 份历史分段，且分段与主文件同属一天、共用保留期（否则分段会绕过保留期永久堆积）；日志目录改为跟随配置目录的实际 root，降级场景下设置页两行路径不再互相矛盾。

**结构**：焦点 Scope 样板单点化，把「为什么 `focus-on-click` 必须是 false」这条历史根因的解释从 6 份复制收敛成 1 份；删除 75 行零引用组件。

**测试**：测试后端从进程级 `init_integration_test_with_mock_time()`（含 proxy，第二个测试线程必然 `AlreadySet` panic）改为**线程局部哨兵 + `init_no_event_loop()`**，这是「断言能拆成独立 `#[test]` 并行跑」的前提；不解决它，七条新断言只能塞进一个聚合测试，失败时无法从测试名直接定位。

---

## 4. 验证方式（这是本轮真正的产出）

### 4.1 四道门禁（每个提交都跑）

| 门禁 | 命令 | 最终结论 |
| --- | --- | --- |
| 格式 | `cargo fmt --all -- --check` | exit 0，无输出 |
| 静态检查 | `pwsh -File tools/clippy.ps1`（本机 MSVC 工具链绕行，等价 `-D warnings`） | exit 0，`Finished dev profile` |
| 类型/全目标 | `cargo check --workspace --all-targets` | exit 0 |
| 测试 | `cargo test --workspace` | `61 passed` + `62 passed` + `3 passed`，0 failed，共 **126** |

### 4.2 七条新断言

`crates/tianyi-app/src/ui_tests.rs` 现共 **8 个 `#[test]`**：1 个聚合的 `ui_geometry_regression` + 7 条本轮新增的独立断言。

| 断言 | 守什么 |
| --- | --- |
| `check_batch_bar_a11y` | D-01：窄档批量栏按钮必须有可读名称 |
| `check_login_input_survives_mode_switch` | D-02：切到扫码再切回，已输入账号不被清空 |
| `check_toast_clears_status_bar` | D-03：Toast 必须让开状态栏传输胶囊 |
| `check_dialog_focus_closure` | A-03：模态焦点闭包（两截，见 §4.3） |
| `check_selection_index_brings_row_into_view` | A-01：`selection-index` 变化必须把该行滚进视口 |
| `check_tooltip_clears_on_page_switch` | D-06：切页必须收掉 Tooltip |
| `check_arrow_key_moves_selection` | A-01 的按键链：方向键真的被派发到列表 |

按模块口径的测试分布（可现查：`cargo test --workspace` 的三条 `running N tests` 输出，与各文件 `#[test]` / `#[tokio::test]` 计数）：app **61**（`controller/mod.rs` 35 / `logging.rs` 10 / `controller/files.rs` 8 / `ui_tests.rs` 8），core **62**（41 个 `#[test]` + 21 个 `#[tokio::test]`），集成 **3**。合计 126。

### 4.3 变异自证

每一项都**实跑**过——把修复人为回退，确认对应断言真的变红。对照表同时写在 `ui_tests.rs` 第 1081–1118 行，便于后人复核「这条断言是否还有判别力」。

| 回退动作 | 变红的断言 | 失败信息要点 |
| --- | --- | --- |
| `cloud.slint`：删掉批量栏 7 处 `tooltip:` | `check_batch_bar_a11y` | 窄档下 `accessible-label` 退化成「按钮」，5 个标签全查不到；宽档对照不受影响，证明判别力来自窄档 |
| `main.slint`：删掉 Toast y 里的 statusbar 补偿 | `check_toast_clears_status_bar` | 下边缘 716→**752**，压过胶囊顶端 **726**，被盖住 26px |
| `main.slint`：`ModalsApi.blocking` 恒 `false` | `check_dialog_focus_closure`（第一截） | 无浮层 / 对话框开 / 对话框关 / 右键菜单开 四个取值全变成 `false`，与期望 `false/true/false/true` 不符 |
| `dialog.slint`：删 `@keys(Tab)`/`@keys(Shift+Tab)`/`@keys(Backtab)` **且** `blocking` 恒 `false` | `check_dialog_focus_closure`（第二截） | Tab×5 + Space 之后 `current-page` 从 0 变成 **2**（期望 0） |
| `cloud.slint`：删 `changed selection-index { … }` 整块 | `check_selection_index_brings_row_into_view` | `selection-index=30` 之后 `row-30` 仍未被实例化，可见行只到 `row-14` |
| `cloud.slint`：删 `@keys(DownArrow)` KeyBinding | `check_arrow_key_moves_selection` | `move-selection` 一次都不响，`selection-index` 停在 −1 |
| `main.slint`：删 `changed current-page` 里的 `TooltipState.visible = false` | `check_tooltip_clears_on_page_switch` | 切页后 `visible` 仍为 `true` |

**本轮唯一一次「按要求做的变异变不红」，也是最重要的一条方法论发现**：A-03 的「`ModalsApi.blocking` 门控」与「对话框 Tab 拦截」是**两道各自都够用的防线**，单独回退任何一道，行为断言都不会红（另一道顶住了）。所以 T4 必须拆成两截：第一截直接把 `ModalsApi.blocking` 在「无浮层 / 对话框开 / 对话框关 / 右键菜单开」的取值钉成 `false/true/false/true`，第二截才验「焦点真的没跑出去」。**只写行为断言，等于给这道修复留了一个永远抓不到退化的盲区。** 更一般的结论：当一个缺陷有两道独立防线时，断言必须能分别证明每一道在场。

诚实记录一处**没做**的变异：T2 的「真回退」是断掉 `pages/login.slint` 的 `text <=> root.user-text`，而该文件不在本轨允许编辑的清单内。替代证据是 T2 断言「切到扫码后原输入框实例 `is_valid() == false`」，以此保证断言走的是「子树被销毁重建」这条真实路径，而不是一次空转往返。

### 4.4 视觉基线

`screenshots/after/` 由 22 张增至 **25 张**（新增 `23-cloud-narrow-actions` 960×760、`24-cloud-inline-error` 1180×760、`25-login-invalid` 900×760）。比对用 **numpy 逐通道**求差，而不是 Pillow 的 `ImageChops.difference(...).getbbox()`（原因见 §5.2）。

预期差异 **3 处**（全部能解释），再加 2 张图上的相位噪声底，**计划外差异 0**：

- `17-toast`：顶边 **668 = 704 − 36**，正是 D-03 的 `statusbar-height + space-2` 偏移；
- `06-cloud-loading`：多了「取消」按钮（`cancel_loading` 接线）；
- `20-login-narrow-captcha`：输入框变红边（`invalid` 接线）；
- 噪声底：`06` 与 `21` 两张含 `animation-tick()` 的 spinner 分别有 **49px / 26px** 的相位噪声——这两张的差异不是回归，同二进制连渲两次即可复现同等幅度。

### 4.5 真机冒烟

`cargo build -p tianyi-app`（debug）→ 启动 12s 无 panic（窗口标题「天翼云盘」）→ `PostMessage(WM_CLOSE)` → `ExitCode = 0` → 日志写入 `%APPDATA%\tianyi-cloud-rust\logs\tianyi-cloud.<日期>.log` → `tasks.json` 中无 `Running`（实测 10 个任务全 `Completed`）→ 日志含 `关窗：0 个任务已归一化为已暂停`。

这次冒烟的诚实边界：采样时**没有在跑的任务**（日志里那个 0 就是这个意思），所以它证明的是「关窗钩子会执行、会落盘、退出码 0」，**不**证明「有任务在跑时的关窗竞态修复」——后者的证据只有单测（见 §7.6）。

---

## 5. 已知偏差与自我修正记录（保留，避免后人重犯）

1. **`18e0f0f` 是跨轨混合提交**：并行子代理共享同一 git 索引，UI 轨道已 `git add` 的 `ui/components/{button,icon-button}.slint` 被核心轨道的 `git commit` 一并带走（`git show --name-only 18e0f0f` 可复核：5 个文件里 2 个属于 UI 轨）。内容未丢、未回退、未改写历史——当时多代理同时在树上，`reset/rebase` 的风险高于历史整洁的收益。此后所有提交改为路径限定 `git commit -m ... -- <paths>`。

2. **`b181cd7` 的提交信息里「22 张逐像素相同」是错误结论**：当时用 `ImageChops.difference(...).getbbox()`，Pillow 12 起该 API 默认 `alpha_only=True`，RGBA 全不透明图恒返回 `None`，于是把「有差异」判成「相同」——这是一次**假绿**。改用 numpy 逐通道重算后，真实结论是「20 张相同 + 2 张 spinner 相位噪声」。因禁止 `--amend`，提交信息未改，此处更正并留档。

3. **S-02 未采用「复用手写提示条于 `FluentInfoBar`」而是删除死组件**：实测替换会产生 **2556** 差异像素（图标字形不同、字号 12→13px、按钮 `min-w: 60px`），要抹平得再给组件加四个参数让它去模仿手写样式——那已经不叫复用。代价明说：`pages/cloud.slint` 的两处提示条仍是手写重复实现。

4. **两份任务书的文件预算算错**：W1 实际 15 个文件（上限写 14）、W2 实际 14 个（上限写 13），均由任务书白名单本身超出所致，执行方守住白名单、未越界。结论：预算要在白名单定稿之后再算，否则「上限」是白名单的副产物而不是约束。

5. **`ModalsApi.blocking` 无法写成组件作用域的全局属性绑定**：Slint 实测报 `Parse error`，改为「派生属性 + `changed` 赋值」，仍是全 UI **唯一**赋值点（这一步换来的是可断言性——第一截断言正是钉这个值的来源）。

6. **同一 FocusScope 内不能有两条相同 `keys` 的 `KeyBinding`**：编译器 `passes/key_bindings.rs` 报错，搜索态 Esc 因此合并进既有 Esc binding，而不是新加一条。

---

## 6. 明确不做的项与理由

沿用上一轮口径，以下七项本轮与上轮都刻意不做：

| 项 | 理由 |
| --- | --- |
| i18n 中英双语 | 需改全部 `.slint` 的每一句文案，diff 巨大、回滚粒度是整个界面；当前文档/注释/目标用户均为中文，改动面与收益严重不成比例 |
| 文件缩略图 | 需要下载缓存 + 并发上限 + 失败降级三件套，否则反而拖慢列表；应单独一轮做 |
| 系统托盘 | 引入新的窗口生命周期语义（最小化到托盘、托盘退出），会与本轮的关窗落盘互相干扰，需专门设计 |
| 列宽拖拽 | 纯增量交互，但与 `detail-level` 三档断点机制耦合，改完需重新校准全部断点 |
| 全局速度限制 | `AppConfig.speed_limit` 已存在，但实现需把令牌桶贯穿到分片级并发路径（core 层改动），风险高于本轮能承担的范围 |
| 任务级失败自动重试 | 与请求级重试语义不同，且需与任务状态机、并发信号量协调，应单独一轮设计与验证 |
| Mica / Acrylic | 未引入 Windows App SDK，且跨平台不可用；保留系统原生标题栏以保证拖动/贴边/双击最大化/DPI 缩放可靠 |

---

## 7. 残余限制（诚实清单）

1. **键盘可达性仍有缺口**：列表可方向键选择、模态有焦点闭包、右键菜单可上下走；**仍缺**账号菜单的 Tab 循环、右键菜单按 Tab 仍可能先跳到 `list-area`（实际键盘路径是方向键 + Enter）、传输行/直链行内部按钮的 Tab 顺序未验证，且**未做真实读屏软件验证**——a11y 断言读的是 accesskit 暴露的同一层，不等价于 NVDA / 讲述人实测。
2. **`FluentMenuItem` 仍用 `button` 角色**（Slint 无 menuitem 角色），读屏会读成按钮而非菜单项。
3. **Tooltip 仍可能残留**：归属键已唯一化，并加了「切页 / 四个 `ListView` 的 `viewport-y` 变化」两个清零点；但**指针静止 + 行被销毁且不滚动**时仍会残留（没有信号可监，已权衡不做「指针位置 × 归属矩形」的复杂方案）。
4. **新发现：960px 断点存在跳变**——窗口 959→960 时导航从折叠变展开，内容区从 **903px 掉到 712px**，于是 **960–1007 区间反而比 959px 更窄**（工具栏丢失标签）。本轮未改：改断点会连带作废 `detail-level` 的三档断言，属单独一轮的响应式重标定。
5. **配置降级分支未能实跑**：Windows 上 `dirs::config_dir()` 走 `SHGetKnownFolderPath`、不读 `APPDATA` 环境变量，无法强制降级，只有单测 + 代码链覆盖。
6. **关窗竞态修复只在「无在跑任务」的真机上验证过**（冒烟时 0 个任务），有任务在跑时的关窗路径仍只有单测覆盖。
7. **真实账号链路仍未回归**（无可用账号）；`T2` 的真回退（断开 `login.slint` 的 `text <=> root.user-text`）不在测试边界内，用「切扫码后旧输入框实例 `is_valid() == false`」作等效证据。
8. **macOS 分支未编译实测**；`dragout` 仍仅 Windows。
9. **`cancel_loading` 在非搜索态只丢弃结果并提示「已取消加载」**，**不会**取消真实网络请求——本轮不引入请求级取消。
10. **场景 `23` 采不到 Tooltip 气泡本体**：需要 450ms 悬停延迟 + 私有 `tip-shown`，无窗口采样会立刻停、强采则 PNG 不可复现，所以它采的是「按钮只剩图标」那一档外观；焦点环同样不被任何基线覆盖。

---

## 8. 提交分布

`fix` 13 / `feat` 6 / `refactor` 3 / `test` 2 / `docs` 1 = **25**（`git log --format=%s 78d90d2..e955f52` 按类型前缀统计可复核）。

按工作流的归属：

| 工作流 | 提交数 | hash |
| --- | --- | --- |
| W4 核心健壮性 | 6 | `67ed4cf` `e2faab3` `ccb61f0` `18e0f0f` `bc5ffda` `4960fad` |
| W1 UI 缺陷 | 8 | `137ef08` `359dd4c` `2c40063` `4048a4e` `e3e087b` `fd27841` `46be5b7` `c600696` |
| W3 结构一致性 | 3 | `b181cd7` `656bfe1` `a585b2f` |
| W2 可访问性 | 5 | `44f5a7d` `3128128` `666fcbb` `38110eb` `6b6d30e` |
| W5 验证与文档 | 3 + 本提交 | `612dec0` `439ab8a` `e955f52` |

文件分布：`crates/tianyi-app/ui/**` **20** 个、`crates/tianyi-core/**` **5** 个、其余源码与配置 **8** 个、`screenshots/after/**` **7** 张（3 新增 + 4 变更），合计 40。

未 push、未 amend 历史，工作分支沿用 `main`。已知偏差两处（`18e0f0f` 跨轨混合提交、`b181cd7` 提交信息里的假绿结论）见 §5.1 与 §5.2，均因并行代理在场时改写历史的收益低于风险而选择保留并登记。
