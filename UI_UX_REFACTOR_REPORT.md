# TianyiCloud-Rust UI/UX 重构报告

> 范围：`crates/tianyi-app`（Slint 界面 + 控制器）为主，`crates/tianyi-core` 未做破坏性改动。
> 技术栈保持 Rust + Slint，纯原生桌面，无 Electron/CEF/WebView；保留 Windows / Linux / macOS 跨平台构建。
> 基线提交：`628826d feat(ui): 按 Fluent Design 2 全量重构界面…`
> 本次结果：28 个文件 +3094/−1155，新增 5 个组件与 1 个无窗口渲染器。

---

## 1. 原有 UI/UX 的主要问题与根因

用户提供的五张截图（登录 / 个人云 / 下载直链 / 设置 / 加载状态）在工作区中不存在，仓库里也没有任何 PNG/JPG 资源。
因此本轮先按**截图描述**定位问题，再用**无窗口软件渲染**把重构前的真实组件树渲染出来复现缺陷
（`screenshots/before/`，来源见第 9 节），确认了下列根因。这不是猜测，`before` 截图与描述逐条对应。

### 1.1 空状态/加载状态错位：把「交叉轴对齐」写成了「主轴对齐」

这是本次最重要的根因。Slint 的 `LinearLayout`：

* `alignment` —— **主轴**对齐（`VerticalLayout` 就是垂直方向），默认 `start`；
* `cross-axis-alignment` —— **交叉轴**对齐，默认 `stretch`。

旧 `components/empty-state.slint` 写的是：

```slint
VerticalLayout {
    alignment: center;   // 只让内容组垂直居中
    Rectangle { width: 56px; height: 56px; }      // 固定宽度 → 交叉轴 stretch 下贴到左边
    Text { ... }                                   // 无宽度约束 → 被拉成整行
    FluentButton { ... }                           // 同样被拉成整行
}
```

于是：图标固定 56px 贴在内容区左侧、标题/描述铺满整行看起来"跑到别处"、主操作按钮被拉伸成接近全宽。
`before/04-cloud-empty.png` 完整复现了这个形态（状态栏里的"传输中 3"胶囊也被拉成了整行）。

附加缺陷：`Text { wrap: word-wrap }` **没有宽度约束就不会换行**，窄窗口下溢出。

### 1.2 半透明颜色全部失效：Slint 的 8 位颜色是 `#RRGGBBAA`，不是 `#AARRGGBB`

旧令牌与组件里写了 `#33000000`、`#1F000000`、`#220F6CBD`、`#CC1A1A1A`（按 AARRGGBB 意图书写）。
Slint 把 8 位字面量解析为 **RRGGBBAA**，因此这些颜色实际是「alpha = 0x00」→ **完全透明**。
本轮用软件渲染做了两次像素验证：

| 写法 | 白色底的渲染结果 | 结论 |
| --- | --- | --- |
| `#80000000` | 255,255,255（无变化） | alpha 在末尾 |
| `#FF000080` | 255,127,127 | 红色 50% 混合，**确认 RRGGBBAA** |

影响面：拖入上传遮罩、二维码过期遮罩、新增对话框 scrim、`shadow` 全部不生效。
已全量改为 alpha 在末尾（`#00000033` / `#0F6CBD22` / `#1A1A1ACC` / `#FFFFFFD9`）。

### 1.3 固定宽度子项贴左、右侧控件被拉伸：`alignment` 默认 `start` + 首选宽度过大

设置页「当前账号」行的"退出登录"按钮被拉成 372px；sheet 里 `space-between` 缺位；
名称表头的排序指示器飘到列尾。根因是 `HorizontalLayout` 默认 `alignment: start`：
当子项首选宽度之和装得下时，所有子项按**首选**尺寸被打包到左侧；而带 `overflow: elide`
的 `Text` 又会被判成"首选宽度很大"，于是触发按 stretch 分配的兜底分支，比例放大后按钮变宽。
本轮统一为：需要某个子项吸收剩余宽度 → `alignment: stretch`；首尾两端对齐 → `alignment: space-between`；
并给所有省略文本加 `max-width`。

### 1.4 侧边栏空间利用率低

容量卡为「标题行 + 进度条 + 说明行」三块 + 10px 内边距（≈100px 高），信息量与占位不成比例；
导航项高 38px、图标 17px、徽章样式不统一。`before/01-cloud.png` 可见大片空白。

### 1.5 状态表达用多个布尔变量互相组合

`file-loading` + `file-error` + `rows.length == 0` + `searching` 四个量交叉决定界面，
必然出现"错误与加载同时为真""加载结束但按钮仍禁用"等矛盾态；
`select-all: bool` + `selected-id: string` 两套选择表达，与后端的"只操作一个文件"强弱不一致。

### 1.6 阻塞 UI 线程的确认框

`delete_selected()` 直接调用 `rfd::MessageDialog::show()`，该 API 在 UI 线程同步阻塞并自建模态循环。

### 1.7 刷新即清空

`refresh_files()` 无条件清空 `file-list` + 选中态，每次 F5/操作后都有一次整列表腾空 + 重建的闪烁。

### 1.8 ScrollView 内 `Math.min(parent.width, …)` 绑定环

设置页重构时暴露：`ScrollView` 的 `viewport-width` 由内容宽度驱动，反向引用 `parent.width`
直接触发 `width → layoutinfo-h → min-width → viewport-width → width` 绑定环（Slint 编译失败）。
已改为基于页面根宽度 `root.width`。

---

## 2. 实施的设计与交互改进

### 2.1 设计令牌（`ui/theme/tokens.slint`）

* 表面 5 级：`window-bg` / `layer-bg` / `card-bg` / `card-bg-alt` / `flyout-bg`，浅深各一套；
* 交互态补齐：`hover-bg` / `pressed-bg` / `selected-bg(-hover)` / `control-bg-disabled` / `focus-ring`；
* 新增 `stroke-control`（输入类描边比分隔线深一档，符合 Fluent）；
* 间距严格 4px 栅格（`space-05…space-8`），字号 10/11/12/13/14/16/20/24/28，字重 400/500/600 令牌化；
* 尺寸：控件 26/32/36、行高 40、表头 44、状态栏 28、导航 248/56、内容最大宽 1040、设置 880、状态文本 380；
* 动效三档（90/180/240ms）且 `reduce-motion` 时归零。

### 2.2 状态组件统一（`components/state-view.slint`）

一个内部 `StateScaffold` + 三个导出组件，版式只实现一次：

```slint
VerticalLayout {
    alignment: center;            // 主轴：内容组垂直居中
    cross-axis-alignment: center; // 交叉轴：每个子项自身水平居中（关键修复）
    // 只留水平 padding：垂直 padding 会让 Slint 的居中产生固定偏移
    Text { width: Math.max(120px, Math.min(root.width - 48px, 380px)); wrap: word-wrap; }
    FluentButton { min-w: 96px; }  // 按内容自适应，不拉伸
}
```

* `FluentEmptyState`：空目录、无搜索结果、无直链、无传输任务；
* `FluentLoadingState`：`animation-tick()` 驱动的环形 Spinner + 说明 + 可选取消；
* `FluentErrorState`：错误原因 + 重试 + 可选次操作。

数值核验（`after/04-cloud-empty.png`，逐像素扫描）：

```
列表区   y = 141 … 715   中心 428
内容 bbox y = 347 … 510   中心 428.5
```

### 2.3 新增可复用组件

| 组件 | 用途 | 说明 |
| --- | --- | --- |
| `components/spinner.slint` | 环形加载指示 | 按 `SpinnerBase` 同一套弧线算法实时计算端点；`Path` 不支持 `rotation-angle` |
| `components/switch.slint` | 布尔设置项 | 40×20 轨道 + 14px 滑块，强调色开启态 |
| `components/dialog.slint` | Fluent 内容对话框 | scrim + 居中卡片 + Esc 取消 / Enter 确认，宽度随窗口收缩 |
| `components/settings-section.slint` | 设置版式原语 | `SettingsSection` / `SettingsRow` / `SettingsDivider` |
| `components/state-view.slint` | 空/加载/错误 | 见 2.2 |
| `examples/ui_preview.rs` | 无窗口视觉回归 | 见第 9 节 |

`empty-state.slint` 已删除（被 `state-view.slint` 取代），避免两套空状态并存。

### 2.4 逐页改进

**侧边栏**：248/56 两档宽度（阈值 960px），品牌行 40px；导航项 36px、选中态 = 左侧 3px 指示条 +
强调淡底 + 600 字重；容量卡压缩到 62px（存储 / 百分比 / 3px 进度条 / 已用·总量），折叠态退化为
单个可悬停查看的存储指示（Tooltip 显示 `存储 41% · 1.62 TB / 4.00 TB`）；账号行 44px，折叠态仅头像。

**个人云 / 家庭云**：命令栏 `alignment: stretch`（面包屑吸收剩余宽度并按最大 200px 省略，搜索框固定宽度，
窄窗口把"新建文件夹"折叠成图标按钮）；**固定高度 36px 操作栏**：无选中时显示"共 N 个项目 + 手写提示 +
全选"，有选中时切换为"已选择 N 项 + 批量操作 + 取消选择"，高度恒定 → 无布局跳动；
表头三列可点击排序（升/降序指示器置于标签前）；非侵入式刷新用 2px 进度条，列表内容不清空。

**传输任务**：行高 72px，方向徽章 + 名称 + 状态徽章（按状态着色）+ 进度条 + 速度/明细/错误；
进度条只在进行中/已暂停时出现（失败/取消/完成不显示，避免误读）；
操作按钮严格按任务状态机出现（暂停/继续/取消/重试/打开目录/移除）；超宽窗口内容限制在 1040px 内居中。

**下载直链**：列表行加图标底色块、名称/URL 两级信息、复制按钮；空状态复用统一组件；
内容宽度限 1040px 居中。

**设置**：页面标题 → 单列 880px 居中 → 分组卡片 → 统一设置行（左名称/说明，右控件，`space-between`）；
主题用分段控件、减少动画用开关、路径行支持省略 + 悬停查看完整路径 + 一键复制 + 同行操作按钮。

**登录**：卡片宽度 `min(432px, 窗口-48px)` 且**水平/垂直都居中**（补 `cross-axis-alignment: center`）；
整页可滚动（最矮窗口 + 验证码 + 多账号 + 错误提示时不裁切）；已保存账号独立分组；
错误就地提示；扫码状态含 loading/waiting/scanned/expired/error 的进度环与文案；密码显隐沿用有效实现。

**外壳**：Toast 提示改为右下角浮层（不参与布局 → 不推动页面），对话框/账号菜单/右键菜单/Toast/Tooltip
统一使用 `flyout-bg` + `radius-flyout` + 边界钳制。

---

## 3. 业务逻辑与架构修改

### 3.1 选择模型（唯一事实来源前移到 Rust）

```rust
selected_ids: Mutex<Vec<String>>,     // 事实来源
selection_anchor: Mutex<String>,      // Shift 范围锚点
entry_cache: Mutex<Vec<FileEntry>>,   // 选择变化只改标记
```

* `FileEntry` 新增 `selected: bool`，由控制器派生写入，UI **只读**；
* `targets()` 从"读 UI 属性（跨线程读 UI）+ `select_all` 布尔"改为纯 Rust 状态按 id 集合解析；
* `range_selection(rows, anchor, target)` 抽成纯函数并覆盖双向/失效锚点测试；
* 刷新后 `retain_selection()` 丢弃已不存在的 id → 异步期间点击旧行不会作用到过期对象。

### 3.2 列表状态机

`list-state ∈ {idle, loading, refreshing, ready, empty, empty-search, error}`，与 `status-text` 同步写入
（`set_list_state(state, status)`），UI 只做 `switch`，不再组合布尔：

```
目录切换 → loading（清选择）      同目录 F5 → refreshing（保留内容与选择）
空结果   → empty / empty-search   失败     → error（有旧数据时用行内 InfoBar，不清空列表）
```

### 3.3 排序

`sort_files(&mut [FileObject], key, asc)` 纯函数：文件夹恒在前；名称忽略大小写；大小数值；修改时间用
接口原始时间串（`yyyy-MM-dd HH:mm:ss` 可字典序比较），**空时间两方向都排最后**。排序在 UI 线程重排
现有行，不发网络请求。

### 3.4 危险操作确认

`PendingConfirm` 动作队列 + 应用内 `FluentContentDialog`；`dialog_confirm()` 才真正执行，
`dialog_cancel()` 什么都不做。移除了会阻塞 UI 线程的 `rfd::MessageDialog::show()`。

### 3.5 修饰键

Slint 的 `TouchArea` 点击事件不带修饰键。用 winit `ModifiersChanged` 同步写入
`ModifiersApi{ctrl,shift,alt}` 全局，列表行在点击回调里读取，实现 Ctrl 切换 / Shift 范围选择。

### 3.6 未改动

认证（Token / sessionSecret / 二维码）、加密、上传下载分片与断点续传（`.part` 合法前缀、分片顺序写入）、
任务状态机（`TaskManager` / `TransferControl`）保持原实现——这些是可靠部分，本轮只在其上做 UI 改造。

---

## 4. 修复的具体 Bug

| # | 缺陷 | 根因 | 修复 |
| --- | --- | --- | --- |
| 1 | 空状态图标贴左、标题/描述错位、按钮被拉成整行 | `alignment` 误当交叉轴；文本无宽度约束 | 交叉轴居中 + 文本限宽 + 按钮内容宽 |
| 2 | 加载状态图标与文字分离、视觉重心偏移 | 同上，且垂直 padding 造成固定偏移 | 统一 `StateScaffold`，去掉垂直 padding |
| 3 | 所有半透明遮罩不生效（拖入遮罩/二维码过期/对话框） | 8 位颜色写成 AARRGGBB | 全量改为 RRGGBBAA |
| 4 | 设置页"退出登录"按钮被拉成 372px | 布局默认 start + 首选宽度被放大 | `SettingsRow` 改 `space-between` + 文本 `max-width` |
| 5 | 名称表头排序指示器飘到列尾 | 主轴剩余空间分配 | 指示器前置 + `alignment: start` |
| 6 | 状态栏"传输中"胶囊被拉成整行 | 同上（`Rectangle` 默认 stretch 参与分配） | 固定内容宽度 + 父行 `alignment: stretch` |
| 7 | 登录卡片窄窗口贴左 | `VerticalLayout` 交叉轴默认 Stretch/Start | 补 `cross-axis-alignment: center` |
| 8 | 设置页编译失败（绑定环） | ScrollView 内引用 `parent.width` | 改用页面根宽度 |
| 9 | 删除确认阻塞 UI 线程 | `rfd::MessageDialog::show()` 同步模态 | 应用内对话框 + 动作队列 |
| 10 | 刷新时列表清空闪烁 + 选择丢失 | `refresh_files` 无条件清空 | `refreshing` 静默刷新，保留内容与选择 |
| 11 | "全选"与"选中一项"状态分裂 | `select_all` 布尔 + `selected_id` 并存 | 统一 `selected_ids` 集合 |
| 12 | 传输失败行仍显示进度条 | 条件写成 `!done` | 仅 running/paused 显示 |
| 13 | 加载期间点击旧行可能操作过期对象 | 用 UI 属性反查目标 | 按 id 集合解析 + `retain_selection` |
| 14 | 冷启动/切换后按钮可能长期禁用 | 加载标志无 finally 语义 | 状态机单一出口，error 也必然写入终态 |

`before/` 与 `after/` 同名截图可直接对照 #1 #2 #5 #6 #7 #8。

---

## 5. 性能与内存

* **移除常驻定时器**：`FluentProgressBar` 不确定态原为 16ms `Timer` 常开，改为 `animation-tick()`
  （只在可见时产生重绘）；新增 Spinner 同样基于 `animation-tick`。
* **进度刷新仍为 200ms 节流**：后台只置脏标记，UI 定时器统一推送（原有机制保留）。
* **排序不重新请求网络**：只重排内存中的行。
* **选择变化不重建文件对象**：`entry_cache` 命中，只改 `selected` 标记后重新发布模型；
  代价是每次点击克隆一次条目列表（O(n) 字符串克隆，千行级约数十微秒），换来实现简单与状态一致，
  已在代码注释中标注取舍。
* **列表虚拟化保留**：全部使用 `ListView`，未改成一次性实例化。
* **序列化与生成数据不落盘**：`examples/ui_preview.rs` 只用进程内演示数据，不读写配置、不发请求。
* **布局副作用**：Toast 改为绝对定位，出现/消失不再触发布局重排；操作栏固定高度消除选中态切换的整页跳动。
* 未新增任何重量级依赖（仅使用已有的 `image`、`slint` 软件渲染器）。

---

## 6. 测试结果与未通过项目

在 Windows + MSYS2 ucrt64 工具链（rustc 1.98.1）下执行：

| 命令 | 结果 |
| --- | --- |
| `cargo check --workspace --all-targets` | 通过 |
| `cargo test --workspace` | 通过：`tianyi-app` 15 passed / `tianyi-core` 41 passed / 0 failed |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通过（0 warning） |
| `cargo fmt --all -- --check` | 通过 |
| `cargo build -p tianyi-app` | 通过 |
| 真机运行 | 启动真实窗口并保持运行 10s 无崩溃、无 panic，日志仅 `starting` |
| 真机截图（150% DPI） | `screenshots/native/01-real-window-login-150dpi.png`（1770×1140 物理 = 1180×760 逻辑） |
| 视觉回归 | `examples/ui_preview` 渲染 21 张场景图，逐张人工核对 + 关键场景像素级核验 |

说明（不假装通过的部分）：

* 本机 PATH 上的 `cargo`/`rustc` 来自 MSYS2，其自带 `cargo-clippy` / `rustfmt`；
  rustup 的 stable 工具链缺少 MSVC 链接器（`link.exe not found`），因此 clippy/fmt 使用
  `C:\msys64\ucrt64\bin\cargo-clippy.exe` 与同目录的 `rustfmt` 执行，与项目构建工具链一致。
* 真实图形窗口已实际启动并截图（见上）。抓图时必须先 `SetProcessDPIAware()` 并把位图尺寸设为
  **物理客户区**尺寸，否则 Windows 会对 `GetWindowRect`/`CopyFromScreen` 做 DPI 虚拟化，
  得到"内容被裁切/疑似错位"的假象——本报告早期就踩过这个坑，最终以物理尺寸 `PrintWindow` 为准。
* 除登录页外，其余页面未在真机上逐页截图（需要真实账号登录）；它们由软件渲染器覆盖。

新增回归测试（`crates/tianyi-app/src/controller.rs::tests`）：

* `sort_files_keeps_folders_first_and_orders_by_key`：三列排序 + 文件夹恒在前 + 双向；
* `sort_files_puts_unknown_time_last_in_both_directions`：空时间恒排最后；
* `range_selection_covers_both_directions`：Shift 范围双向 + 锚点失效退化；
* `files_to_entries_marks_selection_from_authoritative_set`：选中标记来自唯一事实来源；
* 既有测试覆盖：二维码状态映射、条目映射、传输条目进度/错误、清空搜索恢复目录行、中断错误分类、
  文件名净化、目录大小统计等。

---

## 7. 仍然存在的限制

1. **DPI**：登录页已在**真实 150% DPI 窗口**上验证（卡片 648 物理 px = 432 逻辑 px，
   物理客户区中心 885 / 卡片中心 896，未裁切）。其余页面只有软件渲染（1×）验证，
   200% / 125% 以及非 Windows 平台未实测。
2. **系统 Mica / Acrylic 未实现**：保留系统原生标题栏与纯色回退背景，未用半透明纯色冒充系统材质。
3. **登录卡片在极端组合下需要滚动**（最小窗口 620 高 + 验证码 + 多个已保存账号 + 长错误文案），
   已提供整页滚动而不是裁切；未把登录按钮做成粘性页脚。
4. **看不到"修改时间"以外的时间排序依据**：排序直接比较接口原始时间串，若服务端返回非
   `yyyy-MM-dd HH:mm:ss` 格式，排序结果会退化（已在注释和报告中标明，不做猜测性解析）。
5. **键盘导航**：文件列表已绑定 F2 / Delete / F5 / Alt+← / Ctrl+A / Ctrl+F / Esc；未实现控件间的
   Tab 焦点遍历（Slint 的焦点模型限制），因此不能声称完整的键盘可达性。
6. **选择模型每次点击重建模型**（见第 5 节取舍）。
7. **未验证真实网络链路**：本轮没有可用账号，登录/列目录/上传下载的真实交互未回归；
   改动集中在 UI 与状态编排层，未触及传输与认证实现。
8. **`screenshots/before` 的可复现脚本**只保留在本报告中（第 9 节）：它需要检出旧提交并放入
   旧接口版本的渲染器，未作为仓库文件长期维护。

---

## 8. Git 提交记录

```
0d0d6fd test(ui): 无窗口渲染器 + 前后视觉基线截图
edbe425 refactor(app): 选择模型/列表状态机/排序与危险操作确认
7ad22fb refactor(ui): 重建 Fluent 令牌/基础组件并修复空态与加载态布局
```

未提交（按约定不做）：远程推送、PR 创建。工作分支沿用 `main`，未使用 `git reset --hard`，
未覆盖任何用户未提交修改（重构开始时工作区为 clean）。

> 说明：`7ad22fb` 与 `edbe425` 存在依赖关系——UI 树先引入新的属性/回调接口，控制器随后对接。
> 单独检出 `7ad22fb` 时 Rust 侧尚不能编译；两者合并后才是一个可构建状态。

---

## 9. 优化前后截图

渲染方式（可复现）：

```powershell
# 重构后
cargo run -p tianyi-app --example ui_preview -- screenshots/after

# 重构前（在旧提交的 worktree 中执行，渲染器按旧属性接口填写演示数据）
git worktree add ..\tianyi-before 628826d
# 将旧接口版本的 examples/ui_preview.rs 放入该 worktree 后：
cargo run -p tianyi-app --example ui_preview -- <repo>/screenshots/before
```

### 重构前 `screenshots/before/`（12 张）

| 文件 | 场景 |
| --- | --- |
| `01-cloud.png` | 个人云（有数据） |
| `02-cloud-dark.png` | 个人云深色 |
| `04-cloud-empty.png` | **空目录（复现图标贴左 / 按钮拉伸 / 胶囊被拉宽）** |
| `05-cloud-empty-900x620.png` | 空目录（最窄窗口） |
| `06-cloud-loading.png` | 加载态 |
| `09-transfers.png` | 传输任务 |
| `11-links-empty.png` | 下载直链空态 |
| `12-links.png` | 下载直链有数据 |
| `13-settings.png` | 设置 |
| `14-settings-1920x1080.png` | 设置（超宽） |
| `19-login.png` | 登录 |
| `20-login-narrow-captcha.png` | 登录（窄窗 + 验证码 + 错误） |

### 重构后 `screenshots/after/`（21 张）

`01-cloud-light` `02-cloud-dark` `03-cloud-multiselect` `04-cloud-empty` `05-cloud-empty-900x620`
`06-cloud-loading` `07-cloud-error` `08-cloud-search-empty` `09-transfers` `10-transfers-dark`
`11-links-empty` `12-links` `13-settings` `14-settings-1920x1080` `15-settings-dark-1024x768`
`16-dialog` `17-toast` `18-transfer-drawer` `19-login` `20-login-narrow-captcha` `21-login-qr`

重点对照：

| 对照项 | before | after |
| --- | --- | --- |
| 空状态布局 | 图标贴左、文字铺满、无视觉重心 | 图标/标题/说明/按钮垂直居中成组（像素核验中心一致） |
| 空状态主按钮 | 无 / 被拉成整行 | 内容自适应宽度，96px 起 |
| 状态栏传输胶囊 | 被拉伸至近整行 | 内容宽度胶囊 |
| 侧边栏容量卡 | ≈100px，下方大片空白 | 62px 紧凑三行信息 |
| 设置页控件对齐 | 退出登录按钮被拉宽 | 名称左、控件右，`space-between` |
| 登录卡片 | 窄窗口贴左 | 水平垂直居中 + 整页滚动 |
| 传输任务行 | 失败行也显示进度条 | 仅进行中/已暂停显示；状态徽章着色 |

### 真实窗口截图 `screenshots/native/`

| 文件 | 说明 |
| --- | --- |
| `01-real-window-login-150dpi.png` | 真实进程 + 原生标题栏，150% DPI，1770×1140 物理像素 |

抓图脚本要点（PowerShell，可复现）：

```powershell
[PW]::SetProcessDPIAware()            # 关键：否则坐标被 DPI 虚拟化
$cr = GetClientRect($h)               # 物理客户区 1770x1140
$bmp = new Bitmap $cr.R, $cr.B
PrintWindow($h, $hdc, 2)              # PW_RENDERFULLCONTENT
```

> 注意：`screenshots/after/06-cloud-loading.png`、`21-login-qr.png` 含 `animation-tick()` 驱动的
> 加载动画，每次渲染相位不同，文件字节会有一两字节级差异，属预期。


---

## 附录：本轮确认的 Slint 布局与颜色语义（写入代码注释，避免再踩）

1. `alignment` = 主轴对齐，`cross-axis-alignment` = 交叉轴对齐；两者默认值分别是 `start` 与 `stretch`。
2. 交叉轴 `stretch`/`start` 下，**显式宽度的子项贴起始边**；要居中必须写 `cross-axis-alignment: center`。
3. 线性布局的垂直 `padding` 会让「居中」产生固定偏移（内容起点 = `(size-pref)/2 + padding.begin`），
   需要精确居中时不要用对称垂直 padding。
4. `Text` 的 `wrap: word-wrap` 需要显式宽度或 `max-width` 才生效；`overflow: elide` 的文本进布局会放大首选宽度。
5. `ScrollView` 内容不得用 `Math.min(parent.width, …)` 反推宽度（绑定环），改用页面根宽度。
6. 8 位颜色是 `#RRGGBBAA`；`Path` 不支持 `rotation-angle`；`TouchArea` 不提供点击时的修饰键。
7. 真实窗口抓图必须先 `SetProcessDPIAware()` 并按**物理**客户区尺寸建位图，否则得到的是
   DPI 虚拟化后的裁切图，会误导判断（本轮因此一度误判"登录卡片贴左"，做 A/B 后否定）。

### 两个对照实验（记录在案，避免后人重复猜测）

| 实验 | 结果 | 结论 |
| --- | --- | --- |
| 登录页去掉 `cross-axis-alignment: center` | 卡片中心 358（客户区中心 885） | 该属性是**必需**的，就是居中根因 |
| 登录页加 `width: root.width`（怀疑 ScrollView 不撑满） | 卡片中心 896，与不加完全一致 | `ScrollView` 会把内容撑到视口宽度，该改动**不必要**，已回退 |

