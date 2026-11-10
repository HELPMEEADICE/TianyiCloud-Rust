# 界面动效层报告（Fluent / Win11 对齐）

本轮给 `tianyi-app` 的 Slint 界面补一层统一的动效：浮层入场、页面转场、抽屉展开、状态淡入，
以及控件状态过渡。全部走既有 motion 令牌，受「减少动画」开关控制，不新增依赖、不改 Rust 控制器逻辑。

## 1. 令牌与场景

`ui/theme/tokens.slint` 只有三档时长，组件里不允许另写裸时长：

| 令牌 | 值 | 用途 |
|---|---|---|
| `motion-fast` | 90ms | 悬停 / 按下 / 勾选 / 文字与边框换色 |
| `motion-normal` | 180ms | 状态切换 / 页面转场 / 浮层淡入 |
| `motion-slow` | 240ms | 大位移：对话框、Toast、传输抽屉展开 |

缓动约定：一律 `ease-out`；只有布局尺寸变化（导航栏展开/收起）用 `ease-in-out`。
`reduce-motion` 时三档全部归零，组件侧不再各自判断。

## 2. 两条入场配方（本轮最重要的结论）

Slint 的 `animate` 只在属性值**发生变化**时启动，而属性的首次求值是惰性的：

> **在 `init` 回调里把值抬到终值不会产生任何过渡。**
> `init` 在属性绑定建立之后、首次求值之前执行，等到第一次被读到（渲染/布局）时值已经是终值，
> 没有"变化"可言，`animate` 直接不启动。

这不是推测，是本轮实测：抽屉高度写成 `height: 260px * reveal`、`init` 里把 `reveal` 抬到 1 时，
展开高度在第一次布局就是 260（采样 `[260, 260, 260]`），即硬切。改成"下一帧再抬"之后才拿到曲线。

因此全仓统一两条配方：

**配方 A（`if` 创建的元素：对话框 / Toast / 菜单 / Tooltip / 抽屉 / 状态视图 / 徽章）**

```slint
property <float> enter-fade: 0.0;

Timer {
    interval: 1ms;
    running: self.enter-fade < 1.0;   // 抬到 1 之后自己停
    triggered => {
        self.enter-fade = 1.0;
    }
}

opacity: Theme.reduce-motion ? 1.0 : self.enter-fade;   // 减少动画时直接取终值，不留一帧空白

animate opacity {
    duration: Theme.motion-slow;
    easing: ease-out;
}
```

元素每次由 `if` 创建都会带一个新的 Timer，因此每次打开都会重播。
注意 `Timer` 体内的 `self` 指 Timer 自己，必须用外层元素的 id（`toast.` / `menu.` / `ctx.` /
`tooltip.` / `drawer.` / `badge.` / `dot.` / `card.`）或组件根 `root.`。

**配方 B（常驻容器：只有页面容器）**

触发源是业务状态而不是元素创建：`current-page` 变化时先把目标值落到 0，再由 1ms 单次 Timer
在下一帧抬回 1（同一帧里改两次属性不会产生动画）。

```slint
private property <bool> page-shown: true;

page-fade := Timer {
    interval: 1ms;
    running: !root.page-shown;
    triggered => {
        root.page-shown = true;
    }
}

changed current-page => {
    root.page-shown = false;
}
```

配方 B **只允许驱动 `opacity` 这类与几何无关的视觉属性**：Timer 的起点是定时器触发时刻，
几何断言会读到动画起点。

## 3. 本轮清单

| 位置 | 效果 | 时长 | 配方 |
|---|---|---|---|
| `components/dialog.slint` | 遮罩 + 卡片整体淡入 | slow | A |
| `main.slint` Toast | 淡入 + 8px 上滑（y 一起动画） | slow | A |
| `main.slint` 账号菜单 / 右键菜单 | 淡入（**不动位置**） | normal | A |
| `main.slint` Tooltip | 出现那一瞬柔化 | fast | A |
| `main.slint` 传输抽屉 | 高度 0 → 260px + `clip` 拉帘 | slow | A |
| `components/state-view.slint` | 加载 / 空 / 错误三态淡入 | normal | A |
| `pages/login.slint` | 登录卡片淡入 | slow | A |
| `components/nav-item.slint` | 徽章 / 折叠态圆点出现淡入 | fast | A |
| `main.slint` 页面容器 | 切页淡入（fade-through 近似） | normal | B |
| `components/check-box.slint` | 勾选标记 opacity、描边色过渡（原来只有背景在动） | fast | — |
| `components/switch.slint` | 滑块配色过渡（原来只有 x 在动） | fast | — |
| `components/nav-item.slint` | 标签文字换色过渡 | normal | — |
| `components/segmented.slint` | 段文字换色过渡 | fast | — |
| `components/menu.slint` | 菜单行 hover/pressed 背景过渡（此前完全无过渡） | fast | — |

## 4. 为什么不做退场动画

关闭态的元素由 `if` 直接销毁，`animate` 没有第二个帧可以过渡。要做退场必须二选一：
控制器侧把"关闭"延迟到动画结束（引入状态机与竞态），或保留元素加 `visible` 影子层（多一层实例）。
两者都会把一次纯 UI 改动扩散到 Rust 状态机，本轮不做；收起一律即时生效，注释里已写明原因。

## 5. 验证记录

- **动画存在性**：`ui_tests.rs::check_drawer_reveal` 用小步推进 mock 时间 + 每步读一次几何的方式采样，
  抽屉高度曲线为 `0 → 69.06 → 127.09 → 180.61 → 219.70 → 248.40 → 260`（240ms，ease-out），
  断言"必须出现 `0 < h < 260` 的中间值"（防硬切）与"最终正好 260px、页面容器被顶下同样距离"（防不收敛）。
  采样必须每步读一次：Slint 的动画属性只有在被读到时才注册，只推进时间推不动。
- **淡入曲线**：把对话框淡入临时延长到 2000ms 后逐帧采图，遮罩区亮度
  `247.7 → 232.2 → 222.7 → 215.5 → 207.9 → 201.0 → 195.4 → 188.1`（终值 185.3），确认是连续过渡而非跳变。
- **视觉基线**：22 张 `screenshots/after` 重出后，除两张 spinner 相位场景（`06-cloud-loading`、
  `21-login-qr`，同一二进制连渲两次也会差同一区域）外，其余 20 张与改动前**逐像素一致**——
  说明所有入场动效都收敛到了原有终态，没有留下半透明或半高残影。
- **门禁**：`cargo fmt --all -- --check`、`cargo test --workspace --locked`（45 + 58 + 3 全绿）通过。
  `cargo clippy` 在本机无法执行：本机 clippy-driver 与 rustc 版本不一致，默认目标被解析成 MSVC 且缺
  `link.exe`，加 `--target x86_64-pc-windows-gnu` 又报 std 未安装；交由 CI 的 clippy 作业把关。

## 6. 已知取舍

1. 不做缩放：Slint 没有 transform，缩放只能动宽高，而宽高是布局输入，会连带几何断言与居中计算漂移。
2. 右键菜单与账号菜单只淡入、不位移：`check_context_menu` 断言的是绝对位置与四角越界，8px 余量挡不住位移中间态。
3. 列表行不做逐行入场：`ListView` 会复用行实例，滚动时复用行重播淡入会显得"闪"。
4. 抽屉收起无动画、主题切换无交叉淡入（需要给上百个元素各自加 `animate background`，收益不抵成本）。
5. 图标总览组件 `ui/dev/icon-sheet.slint` 未纳入动效范围（仅开发自检用，feature 门控）。
