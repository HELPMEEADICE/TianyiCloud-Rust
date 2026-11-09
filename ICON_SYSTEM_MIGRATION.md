# 图标系统迁移：自绘图标 → Microsoft Fluent UI System Icons

本次改动的目标是把项目里所有「自己画的 UI 图标」换成微软官方图标资源，并修掉截图中
那条异常蓝色胶囊。以下所有结论都对应仓库里的实际代码与已提交的改动，不是建议稿。

---

## 1. 蓝色胶囊的真实根因

### 现象
「下载直链」导航项右侧出现一条蓝色横向胶囊，一直延伸到侧边栏边界并被裁切；
「传输任务」右侧在未选中时也会出现同款灰色胶囊。截图里看不到徽章数字。

### 根因（不是 badge 尺寸写小，也不是图标画错）

`crates/tianyi-app/ui/components/nav-item.slint` 原实现：

```slint
if !root.compact && root.badge > 0: Rectangle {
    x: root.width - 34px;      // 从左往右铺
    y: (root.height - 18px) / 2;
    min-width: 18px;           // ← 只给了 min-width，没有 width
    height: 18px;
    border-radius: 9px;
    ...
}
```

Slint 里**非布局子元素**的 `width` 默认不是「按内容收缩」，而是父容器宽度的 100%
（`Rectangle` 自身不参与内容测量）。于是这个徽章矩形的实际宽度 = 导航项宽度（约 224px），
起点在 `root.width - 34px`：

```
导航项：[............................................]
徽章  ：                              [================= 224px =================]
                                       ↑ 起点 width-34        ↑ 越过右边界后被侧边栏 clip
```

被裁掉之后只剩一条高 18px、圆角 9px 的横向色块 —— 也就是截图里的「蓝色胶囊」。
蓝色/灰色的区别只是 `background: root.selected ? Theme.accent : Theme.control-bg-pressed`，
所以选中「下载直链」时是蓝色，未选中「传输任务」时是灰色。
数字消失是同一个根因的副作用：`Text` 在 224px 宽的矩形里居中，被推到可视区外。

### 修复（改结构，不是挪位置、不是负边距、不是隐藏）

* 宽度按位数显式给定三档：1 位 `18px`、2 位 `26px`、`99+` 为 `34px`；
* 位置改为 `root.width - self.width - 12px`，由**自身宽度**反推，永远落在导航项内部；
* 徽章文字填满父矩形并居中，数字重新可见；
* 导航项标题宽度按徽章实际宽度让位（`root.badge-w + 20px`），长标题不再压到徽章；
* 折叠态「有任务」圆点从 `(width-14, 5)` 收到 `(width-16, 7)`，与 56px 窄栏的 20px 图标同心。

验证方式：无窗口渲染器（`examples/ui_preview.rs`）分别渲染 `12-links`（下载直链选中、badge=2）
与 `09-transfers`（传输任务选中、badge=3），胶囊消失、徽章为完整药丸且数字居中；
`05-cloud-empty-900x620` 覆盖折叠态。该修复是独立提交 `08b6809`，可在迁移提交之前单独回看。

---

## 2. 图标库与资源版本

| 项 | 值 |
| --- | --- |
| 上游 | <https://github.com/microsoft/fluentui-system-icons> |
| 引用分支 | `main` |
| 解析到的提交 | `e15fc0de25fab983e3e42bac8798be69a85c23bf` |
| 许可证 | MIT（`Copyright (c) 2020 Microsoft Corporation`） |
| 画布 | 24×24（1 个图标使用官方 20×20，见下） |
| 实际入库文件 | **112 个 SVG**（56 个语义 × Regular/Filled），合计 **95.5 KB** |
| 位置 | `crates/tianyi-app/ui/assets/icons/` |

只从上游取应用真正用到的图标（没有把整个图标仓库拷进来），并保留官方原始文件名，
便于任何一行 `@image-url` 都能直接对回上游路径。

**关于 20px 的一个例外**：上游 24px 没有 `Sign In` / `Arrow Enter`（逐文件实测 404 → 见下表），
「登录」语义改用官方 **20px** 的 `arrow_enter`。清单支持按图标指定画布尺寸，
`SOURCES.md` 里记录了该图标的画布是 20px。

```text
# 逐文件实测（只举关键几条）
sign_in_24           404
arrow_enter_24       404
arrow_enter_20       200  ← 采用
arrow_enter_16       200
door_arrow_right_24  404
sign_out_24          200  ← 退出登录
```

---

## 3. 替换了哪些图标（语义 → 官方文件名）

| 语义 | 官方图标 | 语义 | 官方图标 |
| --- | --- | --- | --- |
| 个人云 | `home` | 上传 | `arrow_upload` |
| 家庭云 | `people` | 下载 | `arrow_download` |
| 传输任务 | `arrow_swap` | 新建文件夹 | `folder_add` |
| 下载直链 / 链接 | `link` | 删除 | `delete` |
| **设置** | **`settings`** | 重命名 | `rename` |
| 文件夹 | `folder` | 复制 | `copy` |
| 普通文件 | `document` | 移动 | `arrow_move` |
| 图片 | `image` | 打开文件夹 | `folder_open` |
| 视频 | `video` | 全选 | `select_all_on` |
| 音频 | `music_note_2` | 返回 | `arrow_left` |
| 压缩包 | `folder_zip` | 面包屑箭头 | `chevron_right` |
| PDF | `document_pdf` | 下拉/收起 | `chevron_down` / `chevron_up` |
| 文本 | `document_text` | 更多操作 | `more_horizontal` |
| 搜索 | `search` | 关闭 | `dismiss` |
| 刷新 | `arrow_clockwise` | 勾选 | `checkmark` |
| 暂停 | `pause` | 新增 | `add` |
| 继续 | `play` | 筛选 | `filter` |
| 取消 | `dismiss_circle` | 排序/升序/降序 | `arrow_sort` / `arrow_sort_up` / `arrow_sort_down` |
| 重试 | `arrow_counterclockwise` | 完成/成功 | `checkmark_circle` |
| 失败/错误 | `error_circle` | 警告 | `warning` |
| 信息 | `info` | 账号 | `person` |
| 显示密码 | `eye` | 隐藏密码 | `eye_off` |
| 登录 | `arrow_enter`(20px) | 退出登录 | `sign_out` |
| 云 | `cloud` | 云端不可用 | `cloud_off` |
| 存储 | `storage` | 时钟 | `clock` |
| 浅色 | `weather_sunny` | 深色 | `weather_moon` |
| 二维码 | `qr_code` | | |

逐图标映射（含文件名与画布尺寸）见 `crates/tianyi-app/ui/assets/icons/SOURCES.md`。

---

## 4. SVG 如何集成到 Slint（1.17 的真实能力）

### 4.1 为什么是「静态注册表」而不是「运行期拼路径」

`@image-url(...)` 是**编译期**构造，参数必须是字面量。所以「图标名 → 资源」不可能是
运行期字符串拼接，只能是一张编译期静态表。实现为三层，全部由脚本从磁盘真实文件生成：

```slint
// 1) 语义枚举（57 个成员：none + 56）
export enum IconName { none, home, people, ... }

// 2) 资源注册表（112 个 @image-url）
export global IconAssets {
    out property <image> placeholder: @image-url("../assets/icons/ic_fluent_dismiss_24_regular.svg");
    out property <image> home-regular: @image-url("../assets/icons/ic_fluent_home_24_regular.svg");
    out property <image> home-filled:  @image-url("../assets/icons/ic_fluent_home_24_filled.svg");
    ...
}

// 3) Icon 组件里的查表绑定
export component Icon inherits Image {
    in property <IconName> name: IconName.none;
    in property <bool> filled: false;
    in property <bool> disabled: false;
    in property <brush> icon-color: Theme.text-secondary;
    in property <length> size: Theme.icon-md;

    source: root.name == IconName.none ? IconAssets.placeholder
        : root.name == IconName.home ? (root.filled ? IconAssets.home-filled : IconAssets.home-regular)
        ...
        : IconAssets.placeholder;
}
```

一个值得记录的实现细节：最初写成 `IconAssets.pick(name, filled)` 函数，编译器报
`Call of impure function 'pick'` —— 函数体内读了 global，就不是 `pure`，而属性绑定只允许
pure 调用。**查表必须落在 `source` 绑定本身**（绑定会正常建立对 global 的依赖），
这也是最终形态的由来。

### 4.2 着色：`Image.colorize` 是 alpha 蒙版

Slint 的 `Image.colorize` 文档（`i-slint-compiler` 的 `builtins.slint`）明确写的是
「图像作为 alpha 蒙版，按给定颜色绘制」。因此：

* 官方 SVG **一个字节都不用改**，也不需要为每种颜色生成副本；
* 深浅色、选中强调色、禁用灰、危险红全部走 `Theme` 令牌；
* 不会出现「彩色图标被染坏」的问题（只有 alpha 参与着色）。

### 4.3 尺寸与不变形

`Icon` 同时设置 `width: size` / `height: size`（正方形）、`image-fit: contain`、
`image-rendering: smooth`。官方 24×24 画布统一，`contain` 保证不拉伸/不裁切；
SVG 按目标尺寸重新栅格化（不是位图缩放），高 DPI 下依旧清晰。
Regular/Filled 画布一致，切换选中态**不会改变尺寸或位置**。

### 4.4 资源嵌入

`build.rs` 使用 `slint_build::EmbedResourcesKind::EmbedFiles`，SVG 在编译期嵌入二进制。
运行时不需要网络、不需要外部文件、不需要图标字体，也不需要 Node.js。

### 4.5 尺寸令牌

`theme/tokens.slint` 新增四档，替换全部硬编码 px：

| 令牌 | 值 | 用在哪 |
| --- | --- | --- |
| `icon-xs` | 12px | 复选勾、输入框清除、面包屑分隔、微型图标按钮内 |
| `icon-sm` | 16px | 工具栏、按钮前置图标、行内操作、状态栏 |
| `icon-md` | 20px | 导航栏、文件类型图标、传输方向图标、对话框 |
| `icon-lg` | 32px | 空状态、拖入遮罩、品牌位 |

---

## 5. 许可证处理

* `crates/tianyi-app/ui/assets/icons/LICENSE`：上游 MIT 原文（Microsoft Corporation）。
* `crates/tianyi-app/ui/assets/icons/SOURCES.md`：上游仓库、引用分支、解析到的提交号、
  抓取时间、逐图标「语义 → 上游文件名 → 画布 → Regular/Filled 文件名」映射表，
  以及 `colorize` 的说明。
* 仓库根目录原有 `LICENSE`（项目自身）未被修改；图标资源与项目代码许可证分离存放。

---

## 6. 编译与测试结果

```text
cargo fmt --all -- --check                           通过（无输出）
cargo check --workspace                              通过
cargo clippy --workspace --all-targets -- -D warnings 0 警告
cargo test --workspace                               56 passed / 0 failed（15 + 41）
```

clippy 说明：本机是 MSYS2 UCRT64 的 GNU 工具链（`host: x86_64-pc-windows-gnu`），
`cargo-clippy` 版本比 `rustc` 新，直接 `cargo clippy` 会按 MSVC host 解析并报
`linker link.exe not found`。`tools/clippy.ps1` 绕过 `cargo-clippy`，把版本匹配的
`clippy-driver`（0.1.98 / rustc 1.98.1）作为 `RUSTC_WORKSPACE_WRAPPER` + `CLIPPY_ARGS=-Dwarnings`，
等价于要求的那条命令（仅对 workspace 成员生效，不影响依赖）。已用 `-Wclippy::pedantic`
验证 lint 确实在跑（能产出 pedantic 诊断），再切回 `-D warnings` 为 0 警告。

契约检查（PowerShell 统计）：

```text
svg 文件 112 / @image-url 唯一引用 112 / 引用但缺失 0 / 存在但未引用 0
IconName 成员 57（none + 56）/ 查表分支 57
ui/ 下 Slint Path 元素 0、旧 `Icons.` 全局 0、`glyph:` 0、`thickness` 0
```

---

## 7. 视觉验证

`examples/ui_preview.rs`（无窗口软件渲染器）产出 `screenshots/before` 与
`screenshots/after` 各 21 张，覆盖：个人云（浅/深）、多选、空目录、900×620 窄窗口、
加载、错误、搜索无结果、传输任务（浅/深）、下载直链（空/有数据）、设置（普通/1920×1080/
深色 1024×768）、删除确认、Toast、传输抽屉、登录、登录窄窗口+验证码、扫码登录。

`examples/icon_sheet.rs` 另产出 `screenshots/icons/icon-sheet-{light,dark}.png`：
把全部 56 个图标的 Regular/Filled × 16/20/32 六种组合铺开，用于逐个核对官方 SVG
都能正确加载、着色，且没有拉伸或裁切。

重点核对结果：

* 侧边栏四个图标（个人云 / 家庭云 / 传输任务 / 下载直链 / 设置）形状正确，
  「设置」是官方 Settings 齿轮（不再是滑杆拼出来的杂乱形状），选中时切 Filled；
* 下载直链用官方 Link，选中为 Filled Link；
* 文件类型图标（文件夹/文档/图片/视频/压缩包/文本）语义与官方一致；
* 折叠态（56px）图标居中、有任务时显示 6px 圆点、无裁切；
* 深色主题下图标由 `colorize` 正常转为浅色/强调色，无色差；
* **蓝色胶囊完全消失**，徽章为 18/26/34px 三档药丸，数字居中。

---

## 8. Git 提交记录

```text
08b6809  fix(ui): resolve navigation item rendering artifacts
1d9082a  refactor(ui): migrate to Microsoft Fluent System Icons
c2577be  test(ui): verify icon resources and layout behavior
```

每个提交都可独立编译：`08b6809` 只动导航项（旧图标 API 下即可编译），
`1d9082a` 才是图标换库，`c2577be` 只加自检工具与截图。
未使用强制推送，未清空任何既有提交或未提交修改（开工前 `git status` 为干净树）。

---

## 9. 保留的功能

登录、个人云/家庭云、文件管理（含重命名/复制/移动/删除/多选/搜索）、上传下载、
断点续传、下载直链、多账号切换、设置（主题/并发/下载目录）、深浅色主题 ——
Rust 侧逻辑未改动，本次只替换 UI 图标层与导航项布局，`cargo test` 全绿。

---

## 10. 无法验证的部分及原因

1. **真实 150%/200% DPI 的窗口截图**：本机无交互桌面会话可用
   （`MinimalSoftwareWindow` 的 scale factor 固定为 1.0，且没有 setter）。
   已用两条替代证据覆盖：① SVG 按目标尺寸重新栅格化（Slint 内部把物理尺寸传给 resvg），
   与位图放大有本质区别；② 图标总览把同一图标在 16/20/32 三档同时渲染，形状边缘干净、
   无缩放糊边。要拿到真机 DPI 截图，需要在有桌面会话的机器上跑
   `SLINT_SCALE_FACTOR=1.5 cargo run -p tianyi-app`。
2. **真实网络账号下的端到端交互**：本次未登录真实账号，界面数据来自
   `examples/ui_preview.rs` 的演示数据（该文件不读写配置、不发网络请求）。
   图标与布局的验证不依赖真实数据；功能回归以 `cargo test` 的 56 个测试为准。
3. **Linux / macOS 实机**：本机只有 Windows 工具链，未在 Linux/macOS 上运行。
   实现层面无平台相关代码：SVG 走 Slint 内置 resvg（跨平台同一实现），
   资源编译期嵌入，不依赖系统字体或图标字体。
4. **图标字体删除的字节数**：旧实现已经不含字体文件（历史上删过
   MaterialIcons 字体），所以本次没有「体积下降 N KB」这种指标；
   新增体积是 112 个 SVG 共 95.5 KB（编译期嵌入）。
