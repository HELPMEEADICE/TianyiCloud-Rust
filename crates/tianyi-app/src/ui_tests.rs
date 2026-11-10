//! UI 几何回归测试
//!
//! 跑在 Slint 官方的无窗口测试后端（`i-slint-backend-testing`）上：真实组件树、
//! 真实布局，但不开窗口、不渲染、不依赖墙钟 —— 时间由 mock time 推进，
//! 过渡动画用 `mock_elapsed_time` 直接跳过。
//!
//! 断言对象一律是**容器几何**（元素位置 / 尺寸 / 居中关系）。文字的像素宽高随平台
//! 字体度量变化，断言它们只能得到脆弱测试，因此这里一条都没有。
//!
//! 每条断言都在 `900x620`（`ui/main.slint` 声明的 min-width / min-height）与
//! `1180x760`（preferred 尺寸）两档窗口下各跑一遍：布局缺陷往往只在某一档暴露。
//!
//! 关于"一个 `#[test]` 还是多个"：`i-slint-core` 的 `GLOBAL_CONTEXT` 是**线程局部**的
//! （`context.rs`: `thread_local! GLOBAL_CONTEXT : OnceCell<SlintContext>`），而
//! `with_event_loop_proxy` 是**进程级**的。用进程级的 `Once` 装平台时，只有第一个跑到
//! 这里的线程被装上，其它测试线程首次建窗口会走自动初始化分支并拿到
//! `PlatformError::SetPlatformError`（Display 正好是 "The Slint platform was initialized
//! in another thread"）—— 这才是"多个 `#[test]` 并行只有第一个能建窗口"的真正原因。
//! 现在 `boot()` 用**线程局部哨兵**（见 `BACKEND_READY`）让每个测试线程各装一次平台，
//! 于是断言可以拆成独立的 `#[test]`：红的那条自己报出自己的名字，不必再从一堆汇总
//! 信息里挑。A~G 那批历史断言仍集中在一个函数里——它们共用同一份"两档窗口"采样，
//! 拆开只会重复建窗口，没有额外信息。
//!
//! 每条断言各自守住一个历史上真实出现过的布局缺陷，注释里写清了根因，
//! 改 UI 时若把它们改红，先读那条注释再决定是不是真的要改布局。

use i_slint_backend_testing::{AccessibleRole, ElementHandle, ElementQuery};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

/// 窄档：`ui/main.slint` 允许的最小窗口尺寸
const NARROW: (f32, f32) = (900.0, 620.0);
/// 默认档：`ui/main.slint` 的首选窗口尺寸
const WIDE: (f32, f32) = (1180.0, 760.0);

/// 两档窗口尺寸，`(宽, 高)`
const SIZES: [(f32, f32); 2] = [NARROW, WIDE];

/// 云盘页根布局的横向内边距：`ui/pages/cloud.slint` 的 `padding-left/right: Theme.space-4`
const PAGE_PADDING: f32 = 16.0;

/// 状态栏传输胶囊的内容宽度上界（px）：实际内容宽约 84px，这里给字体度量留余量，
/// 同时与"被主轴 stretch 拉宽"后的量级（400px 以上）保持可区分的差距
const CHIP_MAX_WIDTH: f32 = 120.0;

// 本线程是否已经装过测试后端
//
// 为什么必须是线程局部哨兵 + `init_no_event_loop()`：
//  · `GLOBAL_CONTEXT` 是线程局部的（`i-slint-core/context.rs`: `thread_local! GLOBAL_CONTEXT`），
//    所以每个测试线程**可以**各有各的平台；
//  · 但 `set_platform` 里 `EVENTLOOP_PROXY` 是进程级 `OnceLock`（`platform.rs`）：
//    `init_integration_test_with_mock_time()` 带 `threading: true`，会给后端装一个
//    event-loop proxy，于是第二个线程再装平台时直接 `AlreadySet` 失败。
//  · `init_no_event_loop()` 不带 proxy（`new_event_loop_proxy()` 返回 None），
//    因此每个测试线程都能装成 —— 这正是它文档里"each test thread can use its own
//    backend"的意思。本文件不使用事件循环，mock 时间照旧可用。
// 不用进程级 `Once`：那只会给第一个跑到这里的线程装上，其它线程建窗口时报
// "The Slint platform was initialized in another thread"。
// 改成线程局部哨兵后，多个 `#[test]` 才能并行。
thread_local! {
    static BACKEND_READY: Cell<bool> = const { Cell::new(false) };
}

/// 装好本线程的测试后端（可重复调用，同线程只装一次）
fn ensure_backend() {
    BACKEND_READY.with(|ready| {
        if !ready.get() {
            i_slint_backend_testing::init_no_event_loop();
            ready.set(true);
        }
    });
}

/// 初始化测试后端，并返回一个尺寸已设定、布局已落定的主窗口
fn boot(size: (f32, f32)) -> crate::app::MainWindow {
    ensure_backend();
    let ui = crate::app::MainWindow::new().expect("主窗口构造失败");
    // 打包了语言包之后，Slint 会在首个组件创建时按系统 locale 自动选包（匹配策略是
    // "先全等、再只比语言主标签"）：本机 locale 是 zh-CN，而 lang/ 下只有 en / ja /
    // zh-Hant，主标签 zh 就命中了 zh-Hant，界面被翻成繁体。下面是按简中字面量查控件的
    // 几何断言，不钉回索引 0（简中原文）就会全灭。必须在 new() 之后调用。
    crate::i18n::apply(crate::i18n::Lang::ZhHans);
    ui.window()
        .set_size(slint::LogicalSize::new(size.0, size.1));
    ui.show().expect("窗口显示失败");
    settle();
    ui
}
/// 几何回归总入口：A~G 这批历史断言在同一个线程里顺序跑完，失败信息一次性汇总
#[test]
fn ui_geometry_regression() {
    let mut failures: Vec<String> = Vec::new();
    check_login_card(&mut failures);
    check_task_chip(&mut failures);
    check_context_menu(&mut failures);
    check_drawer_reveal(&mut failures);
    check_empty_state_icon(&mut failures);
    check_detail_level_columns(&mut failures);
    check_a11y_semantics(&mut failures);
    check_pointer_routing(&mut failures);
    check_row_open_gestures(&mut failures);
    check_shortcuts_yield_to_rename(&mut failures);
    assert!(
        failures.is_empty(),
        "几何回归失败 {} 项：\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// 推进虚拟时间，让布局与过渡动画落定（不 sleep、不读墙钟）
///
/// 300ms 覆盖最长的一档 `Theme.motion-slow`（240ms）。但注意两个前提：
///  1. 它只对**已经注册**的动画有效。Slint 的绑定是惰性的，动画属性只有在被读到时
///     才注册；而配方 A 的起点是"下一帧"（1ms 单次 Timer），定时器恰好会在下面这次
///     推进的末尾触发。所以对"入场动效 + 几何"这类组合，光调 settle() 不够——
///     参见 `check_drawer_reveal` 里"小步推进 + 每步读一次几何"的采样写法。
///  2. 状态类过渡（hover / 选中 / 折叠）本来就是先有值再变化，settle() 一次就够，
///     现有几何断言全部属于这一类。
fn settle() {
    slint::platform::update_timers_and_animations();
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(300));
    slint::platform::update_timers_and_animations();
}

/// 按 slint 元素 id 取元素
fn by_id(ui: &crate::app::MainWindow, id: &str) -> ElementHandle {
    match ElementQuery::from_root(ui).match_id(id).find_first() {
        Some(element) => element,
        None => panic!("元素 id={id} 不存在"),
    }
}

/// 按组件类型名取元素（匹配的是继承链上的名字）
fn by_type(ui: &crate::app::MainWindow, type_name: &str) -> ElementHandle {
    match ElementQuery::from_root(ui)
        .match_inherits(type_name)
        .find_first()
    {
        Some(element) => element,
        None => panic!("元素类型 type_name={type_name} 不存在"),
    }
}

/// 元素水平中心（绝对坐标）
fn center_x(element: &ElementHandle) -> f32 {
    element.absolute_position().x + element.size().width / 2.0
}

/// 元素右边缘（绝对坐标）
fn right(element: &ElementHandle) -> f32 {
    element.absolute_position().x + element.size().width
}

/// 元素下边缘（绝对坐标）
fn bottom(element: &ElementHandle) -> f32 {
    element.absolute_position().y + element.size().height
}

/// 记录一条失败（不中断其余检查）
fn expect(failures: &mut Vec<String>, ok: bool, message: impl FnOnce() -> String) {
    if !ok {
        failures.push(message());
    }
}

/// 派发一次按键。
///
/// 测试后端只能发"文本形式"的按键，特殊键用的是 Unicode 私用区码点，
/// 取值见 i-slint-common 的 key_codes 表：`Return` = '\n'、`Delete` = '\u{7f}'、
/// `Escape` = '\u{1b}'、`F5` = '\u{f708}'。
fn press_key(ui: &crate::app::MainWindow, ch: char) {
    ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: SharedString::from(ch.to_string()),
    });
    settle();
}

/// A. 登录卡片：宽度被钳制到 `min(432px, 窗口宽 - 48px)`，且水平居中。
///
/// 根因：`ui/pages/login.slint` 里卡片是定宽元素，外层 `VerticalLayout` 的
/// `alignment` 只是**主轴**对齐；只写它时卡片会贴在左边。居中必须靠
/// `cross-axis-alignment: center`，这条断言就是那个历史坑的守门员。
fn check_login_card(failures: &mut Vec<String>) {
    for (width, height) in SIZES {
        let ui = boot((width, height));
        // 未登录态是默认状态，这里显式确认一次，避免将来默认值改动后测试静默失效
        let logged_out = !ui.get_logged_in();

        let card = by_id(&ui, "LoginPage::card");
        let card_size = card.size();
        let card_center = center_x(&card);
        let expected_width = 432.0_f32.min(width - 48.0);
        let window_center = width / 2.0;

        println!(
            "A {width}x{height}: card.width={:.1} (期望 {expected_width:.1}) \
             card.center={card_center:.1} window_center={window_center:.1}",
            card_size.width,
        );

        expect(failures, logged_out, || {
            format!("A {width}x{height}: 断言前置条件失败 —— 主窗口不在未登录态")
        });
        expect(
            failures,
            (card_size.width - expected_width).abs() <= 1.0,
            || {
                format!(
                    "A {width}x{height}: 卡片宽度 {:.1} 与钳制公式算出的 {expected_width:.1} 不符",
                    card_size.width,
                )
            },
        );
        expect(failures, (card_center - window_center).abs() <= 2.0, || {
            format!(
                "A {width}x{height}: 卡片水平中心 {card_center:.1} 偏离窗口中心 {window_center:.1}"
            )
        });
    }
}

/// B. 状态栏传输胶囊：宽度只由内容决定（不参与主轴 stretch），且右边缘贴住内容区右侧。
///
/// 根因：`Rectangle` 在 `HorizontalLayout` 里默认参与主轴 stretch 分配，
/// 少了 `horizontal-stretch: 0` 就会被拉宽 —— 剩余空间在状态文本与胶囊之间分摊，
/// 胶囊吃到大半行（900 宽窗口下约 426px）。
///
/// 只断言"右边缘贴右"抓不到这个缺陷：胶囊是状态栏的**最后一个**子元素，无论是否
/// 参与 stretch 分配，它的右边缘都贴着容器右侧，那条断言在任何宽度下恒真。
/// 真正的判别量是**跨窗口宽度的宽度不变量**：胶囊宽度只由内容决定，窗口从 900
/// 加宽到 1180 时不得变化；一旦它参与 stretch，宽度会随剩余空间一起增长。
fn check_task_chip(failures: &mut Vec<String>) {
    // 逐档实测的 (窗口宽, 胶囊宽)
    let mut widths: Vec<(f32, f32)> = Vec::new();

    for (width, height) in SIZES {
        let ui = boot((width, height));
        ui.set_logged_in(true);
        settle();

        let chip = by_id(&ui, "CloudPage::task-chip");
        let chip_size = chip.size();
        let chip_right = right(&chip);
        // 内容区（导航栏右侧）一直延伸到窗口右边缘，状态栏只剩页内边距
        let expected_right = width - PAGE_PADDING;

        println!(
            "B {width}x{height}: chip.right={chip_right:.1} (期望 {expected_right:.1}) \
             chip.x={:.1} chip.width={:.1}",
            chip.absolute_position().x,
            chip_size.width,
        );

        expect(failures, (chip_right - expected_right).abs() <= 2.0, || {
            format!("B {width}x{height}: 传输胶囊右边缘 {chip_right:.1} 偏离 {expected_right:.1}")
        });
        // 内容宽度的绝对上界：实际内容宽约 84px，120px 给字体度量留足余量，
        // 但与"被主轴拉伸"（约 426px）差一个量级。
        expect(failures, chip_size.width <= CHIP_MAX_WIDTH, || {
            format!(
                "B {width}x{height}: 传输胶囊宽度 {:.1} 超过内容宽度上界 {CHIP_MAX_WIDTH:.0}px",
                chip_size.width,
            )
        });

        widths.push((width, chip_size.width));
    }

    // 跨档不变量：胶囊宽度与窗口宽度无关
    let (narrow_window, narrow_chip) = widths[0];
    let (wide_window, wide_chip) = widths[1];
    println!(
        "B: chip.width 跨档 {narrow_chip:.1}@{narrow_window:.0}x vs {wide_chip:.1}@{wide_window:.0}x \
         差值={:.1}",
        (wide_chip - narrow_chip).abs(),
    );
    expect(failures, (wide_chip - narrow_chip).abs() <= 1.0, || {
        format!(
            "B: 胶囊宽度随窗口宽度变化（{narrow_chip:.1}@{narrow_window:.0}x vs \
             {wide_chip:.1}@{wide_window:.0}x）—— 它参与了主轴 stretch 分配"
        )
    });
}

/// C. 右键菜单：把上下文坐标推到窗口四角，菜单矩形必须始终完整落在窗口内。
///
/// 根因：`ui/main.slint` 的 `ctx` 用 `Math.max(8px, Math.min(context-x, 宽-菜单宽-8px))`
/// 做四角钳制，钳制的上下界任何一个写反 / 漏写都会让菜单跑出窗口。
fn check_context_menu(failures: &mut Vec<String>) {
    for (width, height) in SIZES {
        let ui = boot((width, height));
        ui.set_logged_in(true);
        prepare_context_menu(&ui);
        ui.set_context_open(true);
        settle();

        for (cx, cy) in [(0.0, 0.0), (width, 0.0), (0.0, height), (width, height)] {
            ui.set_context_x(cx);
            ui.set_context_y(cy);
            settle();

            let ctx = by_id(&ui, "MainWindow::ctx");
            let pos = ctx.absolute_position();
            let size = ctx.size();

            println!(
                "C {width}x{height} at ({cx:.0},{cy:.0}): ctx=({:.1},{:.1}) \
                 size={:.1}x{:.1} right={:.1} bottom={:.1}",
                pos.x,
                pos.y,
                size.width,
                size.height,
                right(&ctx),
                bottom(&ctx),
            );

            // 菜单尺寸本身是内容派生的，先挡掉"菜单没被实例化/零尺寸"这种假绿
            expect(failures, size.width > 1.0 && size.height > 1.0, || {
                format!(
                    "C {width}x{height} at ({cx:.0},{cy:.0}): 菜单尺寸异常 {:.1}x{:.1}",
                    size.width, size.height,
                )
            });
            expect(failures, pos.x >= -1.0 && pos.y >= -1.0, || {
                format!(
                    "C {width}x{height} at ({cx:.0},{cy:.0}): 菜单左上角越界 ({:.1},{:.1})",
                    pos.x, pos.y,
                )
            });
            expect(failures, right(&ctx) <= width + 1.0, || {
                format!(
                    "C {width}x{height} at ({cx:.0},{cy:.0}): 菜单右边缘 {:.1} 越界（窗口宽 {width:.1}）",
                    right(&ctx),
                )
            });
            expect(failures, bottom(&ctx) <= height + 1.0, || {
                format!(
                    "C {width}x{height} at ({cx:.0},{cy:.0}): 菜单下边缘 {:.1} 越界（窗口高 {height:.1}）",
                    bottom(&ctx),
                )
            });
        }
    }
}

/// M. 传输抽屉展开动效必须真的"在跑"，并且必须落在 260px。
///
/// 守两件事：
///  1. **动效存在**：抽屉高度是 `260px * reveal`，`reveal` 由 1ms 单次 Timer 在"下一帧"
///     抬到 1（配方 A，见 `ui/theme/tokens.slint`）。若有人改回"在 init 里直接抬值"，
///     Slint 的属性首次求值发生在 init 之后，值一上来就是终值，没有"变化"可言，
///     animate 根本不启动——表现是高度一步跳到 260、看不出过渡。所以这里要求采样里
///     出现过 `0 < h < 260` 的中间值。
///  2. **动效收敛**：最终必须正好 260px，且把页面容器整体顶下去同样距离。
///
/// 采样方式是"小步推进 mock 时间 + 每一步都读一次几何"：Slint 的绑定是惰性的，
/// 动画属性只有在被读到时才会注册/推进，只推进时间而不读是推不动的。
fn check_drawer_reveal(failures: &mut Vec<String>) {
    const DRAWER_HEIGHT: f32 = 260.0;
    const STEP_MS: u64 = 40;
    const MAX_STEPS: usize = 12;

    for (width, height) in SIZES {
        let ui = boot((width, height));
        ui.set_logged_in(true);
        settle();

        // 关闭态：抽屉不存在，页面容器应该顶在内容区最上方
        let page_top_closed = by_id(&ui, "MainWindow::page-host").absolute_position().y;

        ui.set_transfer_panel_open(true);

        // 起点：创建后先读一次，把动画属性定在起始值（0）
        let mut samples = vec![by_id(&ui, "MainWindow::drawer").size().height];
        for _ in 0..MAX_STEPS {
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(STEP_MS));
            slint::platform::update_timers_and_animations();
            let h = by_id(&ui, "MainWindow::drawer").size().height;
            samples.push(h);
            if (h - DRAWER_HEIGHT).abs() <= 1.0 {
                break;
            }
        }

        let drawer = by_id(&ui, "MainWindow::drawer");
        let drawer_size = drawer.size();
        let drawer_pos = drawer.absolute_position();
        let page_top_open = by_id(&ui, "MainWindow::page-host").absolute_position().y;
        let has_intermediate = samples.iter().any(|h| *h > 1.0 && *h < DRAWER_HEIGHT - 1.0);

        println!(
            "M {width}x{height}: drawer=({:.1},{:.1}) {:.1}x{:.1} \
             page_top closed={page_top_closed:.1} open={page_top_open:.1} samples={samples:?}",
            drawer_pos.x, drawer_pos.y, drawer_size.width, drawer_size.height,
        );

        expect(failures, page_top_closed.abs() <= 1.0, || {
            format!("M {width}x{height}: 关闭态页面容器顶端 {page_top_closed:.1} 不在内容区顶部")
        });
        expect(failures, has_intermediate, || {
            format!(
                "M {width}x{height}: 抽屉展开过程中没有中间高度（采样 {samples:?}）\
                 —— 入场动效可能被写成了硬切"
            )
        });
        expect(
            failures,
            (drawer_size.height - DRAWER_HEIGHT).abs() <= 1.0,
            || {
                format!(
                    "M {width}x{height}: 展开后抽屉高度 {:.1} 不等于 {DRAWER_HEIGHT:.1}\
                     （展开动效没有落定？）",
                    drawer_size.height,
                )
            },
        );
        expect(
            failures,
            (page_top_open - DRAWER_HEIGHT).abs() <= 1.0,
            || {
                format!(
                    "M {width}x{height}: 展开后页面容器顶端 {page_top_open:.1} 未被抽屉推到 \
                     {DRAWER_HEIGHT:.1}"
                )
            },
        );
    }
}

/// C 用的小准备：让菜单条目数与"普通目录"一致（菜单高度由内容决定），
/// 名称与 id 都不参与几何断言。
fn prepare_context_menu(ui: &crate::app::MainWindow) {
    ui.set_context_item_name(slint::SharedString::from("几何回归样本"));
    ui.set_context_is_dir(true);
    ui.set_selected_count(1);
}

/// D. 空状态图标：64x64 图标盒的水平中心与列表区水平中心重合。
///
/// 根因：`StateScaffold` 的 `VerticalLayout` 曾经只写 `alignment: center`
/// （那是主轴），于是每个子项被拉伸成父容器全宽、图标被固定在左侧。
/// 现在靠 `cross-axis-alignment: center` 让每个子项自身居中，这条断言守住它。
fn check_empty_state_icon(failures: &mut Vec<String>) {
    for (width, height) in SIZES {
        let ui = boot((width, height));
        ui.set_logged_in(true);
        ui.set_list_state(slint::SharedString::from("empty"));
        let rows_empty = ui.get_file_list().row_count() == 0;
        settle();

        let icon = by_id(&ui, "StateScaffold::icon-box");
        // 空状态组件本身铺满列表区（`ui/pages/cloud.slint` 里 x=0 / width=parent.width），
        // 因此它的水平中心就是列表区的水平中心。
        let list_area = by_type(&ui, "FluentEmptyState");
        let icon_center = center_x(&icon);
        let area_center = center_x(&list_area);

        println!(
            "D {width}x{height}: icon.center={icon_center:.1} icon.size={:.1} \
             list_area=({:.1}..{:.1}) area_center={area_center:.1}",
            icon.size().width,
            list_area.absolute_position().x,
            right(&list_area),
        );

        expect(failures, rows_empty, || {
            format!("D {width}x{height}: 断言前置条件失败 —— 行数据不为空")
        });
        expect(failures, (icon.size().width - 64.0).abs() <= 0.5, || {
            format!(
                "D {width}x{height}: 图标盒宽度 {:.1} 不是 64px",
                icon.size().width,
            )
        });
        expect(failures, (icon_center - area_center).abs() <= 2.0, || {
            format!(
                "D {width}x{height}: 图标水平中心 {icon_center:.1} 偏离列表区中心 {area_center:.1}"
            )
        });
    }
}

/// DE-07 用的宽档：内容区宽度 1352px，四列全开
const EXTRA_WIDE: (f32, f32) = (1600.0, 760.0);

/// 导航栏的两档宽度 `(展开, 折叠)`，**从 UI 读真值**
///
/// D-13 修的就是这里：原来是 `NAV_WIDTH = 248.0` / `NAV_WIDTH_COMPACT = 56.0` /
/// `NAV_COMPACT_BREAKPOINT = 960.0` 三个字面量，是 `ui/` 里真值的副本。UI 一改断点，
/// 测试就静默地按旧数字算内容区，所有断言都跑在错误的坐标上还照样"通过"。
/// 现在四个数（含 `min-capability-width`）全部来自 `Theme` / `LayoutMetrics`。
fn nav_widths(ui: &crate::app::MainWindow) -> (f32, f32) {
    let theme = ui.global::<crate::app::Theme>();
    (theme.get_nav_width(), theme.get_nav_width_compact())
}

/// 导航栏展开/折叠的断点 = `Theme.nav-width + LayoutMetrics.min-capability-width`
///
/// 与 `ui/main.slint` 的 `nav-compact` 绑定表达式逐字对应（248 + 900 = 1148px）：
/// 两个操作数都从 UI 侧读，测试里不出现第三个数字。
fn nav_breakpoint(ui: &crate::app::MainWindow) -> f32 {
    let (expanded, _) = nav_widths(ui);
    expanded
        + ui.global::<crate::app::LayoutMetrics>()
            .get_min_capability_width()
}

/// 内容区可用宽度 = 整窗宽度 − 导航栏宽度（导航档按 `nav_breakpoint` 判）
fn content_width(ui: &crate::app::MainWindow, window_width: f32) -> f32 {
    let (expanded, compact) = nav_widths(ui);
    window_width
        - if window_width < nav_breakpoint(ui) {
            compact
        } else {
            expanded
        }
}

/// E. 次要列分级：断点必须量「内容区宽度」，不是整窗宽度。
///
/// 根因：`ui/main.slint` 曾用 `root.width < 980px ? 0 : …` 分档，而 `root.width` 是整窗
/// 宽度 —— 默认 1180px 窗口的内容区只有 932px，却被判成 2 档（多出"修改时间"列）。
/// 修法：断点改吃 `root.width - 导航栏宽度`。
///
/// 断言方式是"条件元素不存在就不在元素树里"：`if root.detail-level >= n` 分档渲染的三
/// 个列头，在元素查询里找不到就说明该档没开。测试后端不能发键盘事件，但读元素树足够。
fn check_detail_level_columns(failures: &mut Vec<String>) {
    // (窗口宽, 高, 期望 size / time / kind 三列头是否在元素树里)
    let cases: [(f32, f32, bool, bool, bool); 3] = [
        (NARROW.0, NARROW.1, true, false, false),
        (WIDE.0, WIDE.1, true, false, false),
        (EXTRA_WIDE.0, EXTRA_WIDE.1, true, true, true),
    ];

    for (width, height, want_size, want_time, want_kind) in cases {
        let ui = boot((width, height));
        ui.set_logged_in(true);
        settle();
        let content = content_width(&ui, width);

        // 前置条件：没有文件行时列表区仍在，列头也照常渲染（列表状态不影响档位）
        for (id, want) in [
            ("CloudPage::size-header", want_size),
            ("CloudPage::time-header", want_time),
            ("CloudPage::kind-header", want_kind),
        ] {
            let handle = ElementQuery::from_root(&ui).match_id(id).find_first();
            match (&handle, want) {
                (Some(element), true) => {
                    let size = element.size();
                    println!(
                        "E {width}x{height} 内容区 {content:.0}: {id} 存在 {}x{}",
                        size.width, size.height,
                    );
                    expect(failures, size.height > 0.0, || {
                        format!("E {width}x{height} 内容区 {content:.0}: {id} 存在但高度为 0")
                    });
                }
                (None, false) => {
                    println!("E {width}x{height} 内容区 {content:.0}: {id} 不存在（期望）")
                }
                (Some(element), false) => {
                    let size = element.size();
                    expect(failures, false, || {
                        format!(
                            "E {width}x{height} 内容区 {content:.0}: {id} 不该出现（实测 {}x{}）\
                             —— 分档还在用整窗宽度",
                            size.width, size.height,
                        )
                    });
                }
                (None, true) => {
                    expect(failures, false, || {
                        format!(
                            "E {width}x{height} 内容区 {content:.0}: {id} 缺失 \
                             —— 分档把内容区宽度算小了"
                        )
                    });
                }
            }
        }
    }
}

// ==================== F. 可访问性（辅助技术可读名称与角色） ====================

/// 角色枚举的可读名。`AccessibleRole` 是 `#[non_exhaustive]`，匹配必须带兜底分支。
fn role_name(role: Option<AccessibleRole>) -> &'static str {
    match role {
        Some(AccessibleRole::None) => "none",
        Some(AccessibleRole::Button) => "button",
        Some(AccessibleRole::Checkbox) => "checkbox",
        Some(AccessibleRole::Switch) => "switch",
        Some(AccessibleRole::TextInput) => "text-input",
        Some(AccessibleRole::Text) => "text",
        Some(AccessibleRole::Navigation) => "navigation",
        _ => "other",
    }
}

/// 元素的 accessible-label（没有就是空串）
fn label_of(element: &ElementHandle) -> String {
    element
        .accessible_label()
        .map(|label| label.to_string())
        .unwrap_or_default()
}

/// 元素的 accessible-description（没有就是空串）
fn description_of(element: &ElementHandle) -> String {
    element
        .accessible_description()
        .map(|text| text.to_string())
        .unwrap_or_default()
}

/// 按可访问标签查元素。注意 `Text` 也会把自己的文字当成 accessible-label，
/// 所以同一段文字常常同时命中"容器"和它的文字子元素 —— 调用方必须再按角色筛。
fn a11y_hits(ui: &crate::app::MainWindow, label: &str) -> Vec<ElementHandle> {
    ElementHandle::find_by_accessible_label(ui, label).collect()
}

/// 从命中结果里按角色筛，并把每个命中的 (角色, 标签) 打印出来供人眼核对
fn a11y_with_role(hits: &[ElementHandle], tag: &str, role: &str) -> Vec<ElementHandle> {
    println!("F {tag}: 同标签命中 {} 个元素", hits.len());
    let mut picked = Vec::new();
    for element in hits {
        let label = label_of(element);
        let actual = role_name(element.accessible_role());
        println!("F {tag}:   role={actual} label={label:?}");
        if actual == role {
            picked.push(element.clone());
        }
    }
    picked
}

/// F. 可访问性：关键控件必须能被辅助技术**按可访问标签找到**，且角色正确。
///
/// 与 A~E 的几何断言不同，这里断言的是元素树里的可访问语义。每条断言都能因为
/// "没写 accessible-label / accessible-role" 而变红 —— 判据不是"元素存在"，
/// 而是"按标签查得到，且拿到的角色/状态对得上"。
///
/// 复选框的两态不是靠点鼠标（测试后端不能发鼠标事件），而是走辅助技术的
/// `invoke_accessible_default_action()`：它等价于键盘/屏幕阅读器触发的默认动作。
fn check_a11y_semantics(failures: &mut Vec<String>) {
    // ---------- 已登录态：命令栏、搜索框、导航 ----------
    {
        let ui = boot(WIDE);
        ui.set_logged_in(true);
        settle();

        // F1 主命令栏的「上传」按钮：资格来自 root.text，角色 button
        let hits = a11y_hits(&ui, "上传");
        let buttons = a11y_with_role(&hits, "F1 上传", "button");
        expect(failures, buttons.len() == 1, || {
            format!(
                "F1: 按 accessible-label=\"上传\" 找到 {} 个 button（期望 1）\
                 —— FluentButton 的 accessible-role / accessible-label 没生效",
                buttons.len()
            )
        });
        if let Some(button) = buttons.first() {
            let label = label_of(button);
            expect(failures, label == "上传", || {
                format!("F1: 上传按钮的 accessible-label 实测 {label:?}，期望 \"上传\"")
            });
        }

        // F2 搜索框：TextInput 自带 text-input 角色，本任务补的是可读名称
        let hits = a11y_hits(&ui, "搜索云盘文件");
        let inputs = a11y_with_role(&hits, "F2 搜索框", "text-input");
        expect(failures, inputs.len() == 1, || {
            format!(
                "F2: 按 accessible-label=\"搜索云盘文件\" 找到 {} 个 text-input（期望 1）\
                 —— FluentTextField 内层 TextInput 的 accessible-label 没绑到 placeholder",
                inputs.len()
            )
        });
        if let Some(input) = inputs.first() {
            let label = label_of(input);
            expect(failures, label == "搜索云盘文件", || {
                format!("F2: 搜索框的 accessible-label 实测 {label:?}，期望 \"搜索云盘文件\"")
            });
        }

        // F3 导航项「个人云」：角色 button，选中态用描述字段说明"当前页面"
        let hits = a11y_hits(&ui, "个人云");
        let navs = a11y_with_role(&hits, "F3 个人云", "button");
        expect(failures, navs.len() == 1, || {
            format!(
                "F3: 按 accessible-label=\"个人云\" 找到 {} 个 button（期望 1）\
                 —— FluentNavItem 的 accessible-role / accessible-label 没生效",
                navs.len()
            )
        });
        if let Some(nav) = navs.first() {
            let description = description_of(nav);
            expect(failures, description == "当前页面", || {
                format!(
                    "F3: 个人云是选中态，accessible-description 实测 {description:?}，\
                     期望 \"当前页面\""
                )
            });
        }

        // F4 导航栏容器：landmark 角色 navigation
        let hits = a11y_hits(&ui, "主导航");
        let regions = a11y_with_role(&hits, "F4 主导航", "navigation");
        expect(failures, regions.len() == 1, || {
            format!(
                "F4: 按 accessible-label=\"主导航\" 找到 {} 个 navigation 角色元素（期望 1）",
                regions.len()
            )
        });
    }

    // ---------- 未登录态：登录页的「保存账号」复选框 ----------
    {
        let ui = boot(WIDE);
        settle();

        let hits = a11y_hits(&ui, "保存账号");
        let checkboxes = a11y_with_role(&hits, "F5 保存账号", "checkbox");
        expect(failures, checkboxes.len() == 1, || {
            format!(
                "F5: 按 accessible-label=\"保存账号\" 找到 {} 个 checkbox（期望 1）\
                 —— FluentCheckBox 的 accessible-role / accessible-label 没生效",
                checkboxes.len()
            )
        });
        if let Some(checkbox) = checkboxes.first() {
            // 登录页 save-account 默认为 true：初值必须读得出来
            let first = checkbox.accessible_checked();
            println!("F5 保存账号: 初始 accessible-checked={first:?}");
            expect(failures, first == Some(true), || {
                format!(
                    "F5: 初始 accessible-checked 实测 {first:?}，期望 Some(true) —— \
                     accessible-checked 没绑定到 root.checked"
                )
            });

            // 走辅助技术的默认动作翻一次，必须真的变成未选中
            checkbox.invoke_accessible_default_action();
            settle();
            let second = checkbox.accessible_checked();
            println!("F5 保存账号: 触发默认动作后 accessible-checked={second:?}");
            expect(failures, second == Some(false), || {
                format!("F5: 触发默认动作后 accessible-checked 实测 {second:?}，期望 Some(false)")
            });

            // 再翻回来，确认两态都跟得住
            checkbox.invoke_accessible_default_action();
            settle();
            let third = checkbox.accessible_checked();
            println!("F5 保存账号: 再翻一次后 accessible-checked={third:?}");
            expect(failures, third == Some(true), || {
                format!(
                    "F5: 第二次触发默认动作后 accessible-checked 实测 {third:?}，期待 Some(true)"
                )
            });
        }
    }

    // ---------- 设置页：只有标签、自己没有文字的「减少动画」开关 ----------
    {
        let ui = boot(WIDE);
        ui.set_logged_in(true);
        ui.set_current_page(4);
        settle();

        let hits = a11y_hits(&ui, "减少动画");
        let switches = a11y_with_role(&hits, "F6 减少动画", "switch");
        expect(failures, switches.len() == 1, || {
            format!(
                "F6: 按 accessible-label=\"减少动画\" 找到 {} 个 switch（期望 1）\
                 —— 设置页没给 FluentSwitch 传 accessible-label，或它没声明 switch 角色",
                switches.len()
            )
        });
        if let Some(toggle) = switches.first() {
            let checked = toggle.accessible_checked();
            println!("F6 减少动画: accessible-checked={checked:?}");
            expect(failures, checked.is_some(), || {
                "F6: 减少动画开关的 accessible-checked 读不到 —— 没绑定到 root.checked".to_string()
            });
        }
    }
}

// ==================== G. 指针与键盘的派发路径（点击必须一次到位） ====================
//
// 与 A~F 的断言不同，这一组断言的是**事件到底落到了谁身上**：用测试后端把真实的
// 按下/抬起与按键事件派发进窗口，再看组件的回调有没有响。
//
// 曾出现过、且必须继续守住的三类缺陷：
//   G1/G2  FocusScope 的 focus-on-click（默认 true）把第一次按下当成"只给焦点"并
//          终止派发（`FocusScope::input_event` 返回 EventAccepted），覆盖在
//          TouchArea 之上时控件要点第二次才响应 —— 左侧导航栏、右键菜单项都是这个症状。
//   G3     整表替换 `file-list` 模型会重建全部行实例，Slint 的点击计数随之清零
//          （命中项一变就 `set_click_count(0)`），双击永远凑不齐 —— 表现为"双击打不开"。
//   G4/G5  列表焦点没回到文件列表时，Enter 这类快捷键会打在空气里。
//
// 这一组只在首选窗口尺寸下跑：断言的是派发路径，与窗口宽度无关（几何断言才需要两档）。

/// 按 `accessible-label` 在一类组件里查元素（同标签可能同时命中容器与文字子元素，
/// 这里用 `match_inherits` 限定组件类型）
fn labelled(ui: &crate::app::MainWindow, type_name: &str, label: &str) -> Option<ElementHandle> {
    ElementQuery::from_root(ui)
        .match_inherits(type_name)
        .find_all()
        .into_iter()
        .find(|element| label_of(element) == label)
}

/// 构造一行文件列表数据
fn file_row_data(id: &str, name: &str, is_dir: bool, selected: bool) -> crate::app::FileEntry {
    crate::app::FileEntry {
        id: SharedString::from(id),
        name: SharedString::from(name),
        size: 0,
        size_text: SharedString::from("—"),
        is_dir,
        modified: SharedString::default(),
        kind: SharedString::from("other"),
        kind_text: SharedString::from("文件夹"),
        selected,
    }
}

/// 一行表格数据（两行的目录，id 稳定）
fn two_row_model(selected: Option<&str>) -> ModelRc<crate::app::FileEntry> {
    ModelRc::new(VecModel::from(vec![
        file_row_data("1", "文件夹A", true, selected == Some("1")),
        file_row_data("2", "文件夹B", true, selected == Some("2")),
    ]))
}

/// 第一行 FileRow 元素
fn first_row(ui: &crate::app::MainWindow) -> ElementHandle {
    ElementQuery::from_root(ui)
        .match_inherits("FileRow")
        .find_first()
        .expect("文件列表没有渲染出任何 FileRow")
}

/// 记录型回调：测试里用它判断"某个动作到底有没有被触发"
type Hits = Rc<RefCell<Vec<String>>>;

fn record(hits: &Hits, tag: &'static str) -> impl Fn() + 'static {
    let hits = hits.clone();
    move || hits.borrow_mut().push(tag.to_string())
}

fn hit_list(hits: &Hits) -> Vec<String> {
    hits.borrow().clone()
}

/// G1/G2. 单击必须一次到位：导航项切页、右键菜单项执行动作。
///
/// 根因（两处同一个坑）：`FocusScope` 的 `focus-on-click` 默认 true，且这些组件的
/// FocusScope 覆盖整个控件、声明在 TouchArea 之后（命中测试在前）。
/// 按下时它返回 `EventAccepted` 直接终止派发，TouchArea 收不到这次点击，
/// 用户看到的就是"左侧导航要点两下""右键菜单点不动"。
fn check_pointer_routing(failures: &mut Vec<String>) {
    // ---------- G1 导航项：单击即切页 ----------
    let ui = boot(WIDE);
    ui.set_logged_in(true);
    settle();

    let nav = labelled(&ui, "FluentNavItem", "设置").expect("导航项「设置」不存在");
    nav.mock_single_click(PointerEventButton::Left);
    settle();
    let page = ui.get_current_page();
    println!("G1 单击导航「设置」: current-page={page}");
    expect(failures, page == 4, || {
        format!(
            "G1: 单击导航「设置」后 current-page 实测 {page}（期望 4）\
             —— 第一下点击被 FocusScope 的 focus-on-click 吃掉了，要点第二次才生效"
        )
    });

    // ---------- G2 右键菜单项：单击即触发动作 ----------
    let hits: Hits = Rc::new(RefCell::new(Vec::new()));
    ui.on_select_all(record(&hits, "select-all"));
    ui.set_current_page(0);
    ui.set_context_item_name(SharedString::from("样本"));
    ui.set_context_is_dir(true);
    ui.set_selected_count(1);
    ui.set_context_x(200.0);
    ui.set_context_y(200.0);
    ui.set_context_open(true);
    settle();

    let item = labelled(&ui, "FluentMenuItem", "全选").expect("右键菜单项「全选」不存在");
    item.mock_single_click(PointerEventButton::Left);
    settle();
    let fired = hit_list(&hits);
    println!(
        "G2 单击菜单项「全选」: 回调={fired:?} context-open={}",
        ui.get_context_open()
    );
    expect(failures, fired == vec!["select-all".to_string()], || {
        format!(
            "G2: 单击右键菜单项「全选」触发的回调实测 {fired:?}（期望 [\"select-all\"]）\
             —— 菜单项的 FocusScope 把第一下点击吞掉了"
        )
    });
    expect(failures, !ui.get_context_open(), || {
        "G2: 点了菜单项之后 context-open 仍为 true（菜单没关闭）".to_string()
    });

    // ---------- G2b 点击之后键盘仍能激活该按钮（鼠标点击必须把焦点交给控件自己） ----------
    let uploads: Hits = Rc::new(RefCell::new(Vec::new()));
    ui.on_upload_files(record(&uploads, "upload"));
    let button = labelled(&ui, "FluentButton", "上传").expect("命令栏的「上传」按钮不存在");
    button.mock_single_click(PointerEventButton::Left);
    settle();
    expect(failures, !hit_list(&uploads).is_empty(), || {
        "G2b: 单击「上传」按钮没有触发 upload".to_string()
    });
    uploads.borrow_mut().clear();
    press_key(&ui, ' ');
    let by_keyboard = hit_list(&uploads);
    println!("G2b 单击「上传」后按 Space: 回调={by_keyboard:?}");
    expect(failures, by_keyboard == vec!["upload".to_string()], || {
        format!(
            "G2b: 鼠标点过「上传」之后按 Space 触发的回调实测 {by_keyboard:?}\
             （期望 [\"upload\"]）—— 点击没有把焦点交给按钮自己的 FocusScope"
        )
    });
}

/// G3. 双击打开：**选中态更新不能重建行实例**，否则双击判定永远凑不齐。
///
/// 这里刻意走控制器真正用的那条路径（`apply_entries` 就地更新），而不是直接
/// 换模型：正是"每次点击都换一个新 VecModel"让第一下点击之后行实例被重建，
/// 第二下落在新实例上，Slint 的点击计数被清零，`double-clicked` 永远不触发。
fn check_row_open_gestures(failures: &mut Vec<String>) {
    let ui = boot(WIDE);
    ui.set_logged_in(true);
    ui.set_current_page(0);

    let opened: Hits = Rc::new(RefCell::new(Vec::new()));
    let o = opened.clone();
    ui.on_open_item(move |id, _, _| o.borrow_mut().push(id.to_string()));

    ui.set_file_list(two_row_model(None));
    settle();

    // 模拟控制器 handle_select_item -> set_selection 的那一步：选中态就地更新
    let entries = vec![
        file_row_data("1", "文件夹A", true, true),
        file_row_data("2", "文件夹B", true, false),
    ];
    let before = first_row(&ui);
    crate::controller::apply_entries(&ui, entries, 1);
    settle();
    expect(failures, before.is_valid(), || {
        "G3: 选中态更新之后原来的 FileRow 实例失效了 —— 说明走了整表换模型，\
         双击的两下会落在不同实例上"
            .to_string()
    });

    // 两下点击（mock 时间相隔 50ms，在 500ms 的双击窗口内）
    first_row(&ui).mock_single_click(PointerEventButton::Left);
    settle();
    first_row(&ui).mock_single_click(PointerEventButton::Left);
    settle();
    let opened_rows = hit_list(&opened);
    println!("G3 双击第一行: open-item={opened_rows:?}");
    expect(failures, opened_rows == vec!["1".to_string()], || {
        format!(
            "G3: 双击第一行触发的 open-item 实测 {opened_rows:?}（期望 [\"1\"]）\
             —— 行实例在两次点击之间被重建，双击判定失效"
        )
    });

    // ---------- G4 Enter = 打开唯一选中项 ----------
    let keys: Hits = Rc::new(RefCell::new(Vec::new()));
    ui.on_open_selected(record(&keys, "open-selected"));
    ui.set_file_list(two_row_model(Some("1")));
    ui.set_selected_count(1);
    settle();
    first_row(&ui).mock_single_click(PointerEventButton::Left);
    settle();
    press_key(&ui, '\n');
    let enter_hits = hit_list(&keys);
    println!("G4 Enter: 回调={enter_hits:?}");
    expect(
        failures,
        enter_hits == vec!["open-selected".to_string()],
        || {
            format!(
                "G4: 列表里按 Enter 触发的回调实测 {enter_hits:?}（期望 [\"open-selected\"]）\
             —— list-area 没有 @keys(Return) 绑定，或焦点不在列表上"
            )
        },
    );

    // ---------- G5 右键菜单关掉之后，Enter 仍然有效 ----------
    keys.borrow_mut().clear();
    ui.set_context_open(true);
    settle();
    press_key(&ui, '\u{001b}');
    expect(failures, !ui.get_context_open(), || {
        "G5: 右键菜单打开时按 Esc 没关掉菜单".to_string()
    });
    keys.borrow_mut().clear();
    press_key(&ui, '\n');
    let after_close = hit_list(&keys);
    println!("G5 关菜单后 Enter: 回调={after_close:?}");
    expect(
        failures,
        after_close == vec!["open-selected".to_string()],
        || {
            format!(
                "G5: 右键菜单关闭后按 Enter 触发的回调实测 {after_close:?}\
             （期望 [\"open-selected\"]）—— 焦点没还给文件列表"
            )
        },
    );
}

/// G6. 重命名输入框有焦点时，列表的 Enter / Delete / Esc 必须让位。
///
/// KeyBinding 在**捕获阶段**判定，匹配上就把按键吃掉；把开关写在 `activated`
/// 里的 if 上已经太晚了。曾经因此出现"重命名时按 Delete 把选中文件删了"。
fn check_shortcuts_yield_to_rename(failures: &mut Vec<String>) {
    let ui = boot(WIDE);
    ui.set_logged_in(true);
    ui.set_current_page(0);

    let hits: Hits = Rc::new(RefCell::new(Vec::new()));
    let h = hits.clone();
    ui.on_rename_submitted(move |_, name| h.borrow_mut().push(format!("rename-submitted:{name}")));
    let h = hits.clone();
    ui.on_delete_selected(move || h.borrow_mut().push("delete-selected".into()));
    let h = hits.clone();
    ui.on_open_selected(move || h.borrow_mut().push("open-selected".into()));
    let h = hits.clone();
    ui.on_clear_selection(move || h.borrow_mut().push("clear-selection".into()));
    let h = hits.clone();
    ui.on_rename_cancelled(move |_| h.borrow_mut().push("rename-cancelled".into()));

    // 行实例先以"不在重命名"的状态存在，再切 renaming-id：
    // `changed renaming` 只在真的变化时触发，初始值不触发，输入框也拿不到焦点。
    ui.set_renaming_id(SharedString::default());
    ui.set_file_list(two_row_model(Some("1")));
    ui.set_selected_count(1);
    settle();
    ui.set_renaming_id(SharedString::from("1"));
    settle();

    for (name, ch) in [("Enter", '\n'), ("Delete", '\u{007f}'), ("Esc", '\u{001b}')] {
        hits.borrow_mut().clear();
        press_key(&ui, ch);
        let fired = hit_list(&hits);
        println!("G6 重命名中 {name}: 回调={fired:?}");
        let stolen = fired.iter().any(|tag| {
            tag == "delete-selected" || tag == "open-selected" || tag == "clear-selection"
        });
        expect(failures, !stolen, || {
            format!("G6: 重命名输入框里按 {name} 触发了列表快捷键 {fired:?} —— KeyBinding 没在 enabled 上让位")
        });
    }

    // Enter 必须交给输入框：提交重命名
    hits.borrow_mut().clear();
    press_key(&ui, '\n');
    let submitted = hit_list(&hits);
    expect(
        failures,
        submitted.iter().any(|t| t.starts_with("rename-submitted")),
        || {
            format!(
                "G6: 重命名输入框里按 Enter 没有提交重命名（回调实测 {submitted:?}）\
                 —— 按键被列表的 @keys(Return) 吃掉了"
            )
        },
    );

    // Esc 同理：必须是"取消重命名"，不是"清空选择"
    hits.borrow_mut().clear();
    press_key(&ui, '\u{001b}');
    let cancelled = hit_list(&hits);
    expect(
        failures,
        cancelled == vec!["rename-cancelled".to_string()],
        || {
            format!(
            "G6: 重命名输入框里按 Esc 触发的回调实测 {cancelled:?}（期望 [\"rename-cancelled\"]）\
             —— 按键被列表的 @keys(Escape) 吃掉了"
        )
        },
    );
}

// ==================== H. 前三轨修复的变异守卫 ====================
//
// 这一组与前六组的分工：A~G 记的是"当年踩过的坑长什么样"，H 记的是**哪一种回退会
// 让它们重新出现**。每条断言都必须能在对应修复被回退时变红——变不红的断言不算数，
// 所以每条函数的注释里都写清了"回退什么 → 红在哪里"。
//
// 全部按独立 `#[test]` 拆开：失败时测试名本身就是结论（见文件头的线程局部后端说明）。
//
// ---------------------------------------------------------------------------
// 变异对照表（每一项都**实测跑过**，不是推演；结果记录于本轮轨道报告）
// ---------------------------------------------------------------------------
//
// 回退动作                                              → 变红的断言
// ─────────────────────────────────────────────────────────────────────────────
// cloud.slint：删掉批量栏 7 处 `tooltip:`                → check_batch_bar_a11y
//   （窄档下 accessible-label 退化成 "按钮"，5 个标签全查不到；宽档对照不受影响，
//     这正是"判别力来自窄档"的证据）
// main.slint：删掉 Toast y 里的 statusbar 补偿           → check_toast_clears_status_bar
//   （下边缘 716→752，压过胶囊顶端 726，被盖住 26px）
// main.slint：`ModalsApi.blocking = false`（恒 false）  → check_dialog_focus_closure
//   （第一截"blocking 取值"立刻红：无浮层/对话框开/关/右键菜单开 全是 false）
// dialog.slint：删掉 @keys(Tab)/@keys(Shift+Tab)/@keys(Backtab) **且** blocking 恒 false
//                                                     → check_dialog_focus_closure
//   （第二截红：Tab×5 + Space 把 current-page 从 0 切到 2。这一档才是 A-03 之前的
//     真实状态。**单独**去掉其中任何一道防线都会保持绿色——两道防线各自都够用，
//     这也是 T4 必须有第一截的原因：行为断言抓不到"只回退一道"）
// cloud.slint：删掉 `changed selection-index { … }`     → check_selection_index_brings_row_into_view
//   （row-30 始终没被实例化，可见行停在 row-00…row-14）
// cloud.slint：删掉 `@keys(DownArrow)` KeyBinding       → check_arrow_key_moves_selection
//   （move-selection 一次都不响，selection-index 停在 -1）
// main.slint：删掉 `changed current-page` 里的          → check_tooltip_clears_on_page_switch
//   `TooltipState.visible = false`                       （切页后 visible 仍为 true）
//
// 没有覆盖到的回退（诚实记录）：
//  · T2（登录输入保留）的"真回退"是 `pages/login.slint` 里把 `text <=> root.user-text`
//    断掉。这个文件不在本轮轨道允许编辑的清单里，所以没做运行时变异；作为替代，
//    T2 断言了"切到扫码后**原输入框实例必须失效**"，从而保证这条断言走的确实是
//    "子树被销毁重建"这条路径，而不是一次空转的往返。

/// 在小步推进的 mock 时间里反复采样同一个几何量，直到它不再变化（动效收敛）
///
/// 与 `check_drawer_reveal` 同一套采样纪律：Slint 的绑定是惰性的，动画属性只有被
/// 读到才会注册/推进，所以"只推进时间不读"推不动动画。不 sleep、不读墙钟。
fn settle_until(mut sample: impl FnMut() -> f32) -> f32 {
    const STEP_MS: u64 = 40;
    const MAX_STEPS: usize = 15;
    let mut last = sample();
    for _ in 0..MAX_STEPS {
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(STEP_MS));
        slint::platform::update_timers_and_animations();
        let now = sample();
        if (now - last).abs() < 0.25 {
            return now;
        }
        last = now;
    }
    last
}

/// 在某类组件里按可读名称数命中数（`labelled` 的"数一数有几个"版本）
fn count_labelled(ui: &crate::app::MainWindow, type_name: &str, label: &str) -> usize {
    ElementQuery::from_root(ui)
        .match_inherits(type_name)
        .find_all()
        .iter()
        .filter(|element| label_of(element) == label)
        .count()
}

/// 按占位文本找 `text-input` 角色的输入框
///
/// 优先比对 `accessible-placeholder-text()`（这是**输入框自己的**语义属性），
/// 该属性读不到时退回 `accessible-label()`（`components/text-field.slint` 把
/// placeholder 绑成了可读名称），两条通道任一成立即算命中。
fn text_input_by_placeholder(
    ui: &crate::app::MainWindow,
    placeholder: &str,
) -> Option<ElementHandle> {
    let wanted = placeholder.to_string();
    let wanted_for_predicate = wanted.clone();
    ElementQuery::from_root(ui)
        .match_accessible_role(AccessibleRole::TextInput)
        .match_predicate(move |element| {
            let by_placeholder = element
                .accessible_placeholder_text()
                .map(|text| text.to_string())
                .unwrap_or_default();
            by_placeholder == wanted_for_predicate || label_of(element) == wanted_for_predicate
        })
        .find_first()
}

/// 当前**被实例化**的文件行名称（ListView 只实例化视口内的行）
fn instantiated_row_labels(ui: &crate::app::MainWindow) -> Vec<String> {
    ElementQuery::from_root(ui)
        .match_inherits("FileRow")
        .find_all()
        .iter()
        .map(label_of)
        .collect()
}

/// T1（守 D-01）窄档批量栏的按钮必须仍有可读名称。
///
/// 缺陷：批量操作按钮在窄档下把 `text:` 清空只留图标，可读名称与悬停说明就只能来自
/// `tooltip:`。D-01 之前那六个按钮一个 tooltip 都没有，屏幕阅读器在窄档下读到的是
/// 五个一模一样的无名按钮，用户无法分清"删除"和"取消选择"。
///
/// 为什么必须用 900–955px 这个窗口区间：`CloudPage.narrow` 量的是**内容区**宽度
/// （整窗 − 导航栏），判据是 `LayoutMetrics.min-capability-width`（900px）：
///   · 900px  → 导航折叠 56px → 内容区 844px → 图标档 ✅（本轮**唯一**的图标档区间）
///   · 955px  → 内容区 899px → 仍是图标档
///   · 956px  → 内容区 900px → 宽档（标签回来了）
///   · 1180px → 内容区 932px → 宽档
/// 所以 900px 是能触发图标档的窗口区间（900–955）里最靠边的一档，用 1180 测这条断言
/// 等于什么都没测。此前这里用 960×760——那是旧断点（整窗 960 折叠 / 内容区 760 分档）
/// 下唯一能触发窄档的窗口区间，两个断点不同源时才会出现那种"窗口越宽能力越窄"的
/// 怪区间（D-11）。
///
/// 回退什么会红：把 `pages/cloud.slint` 批量栏那六处 `tooltip:` 删掉 → 窄档下
/// `FluentButton.accessible-label` 退化成兜底值 "按钮" → 按 "复制"/"删除" 等标签
/// 一个都查不到（不是"变成空串"：`components/button.slint` 的兜底是 "按钮"，
/// 断言直接比标签内容，两种退化都红）。
#[test]
fn check_batch_bar_a11y() {
    let mut failures: Vec<String> = Vec::new();

    // ---- 图标档：900x620（导航折叠 56px，内容区 844px < 900px） ----
    let ui = boot(NARROW);
    ui.set_logged_in(true);
    ui.set_crumbs(ModelRc::new(VecModel::from(vec![
        crate::app::CrumbEntry {
            id: SharedString::from("-11"),
            name: SharedString::from("个人云"),
        },
        crate::app::CrumbEntry {
            id: SharedString::from("d1"),
            name: SharedString::from("工作文档"),
        },
    ])));
    let rows: Vec<crate::app::FileEntry> = (0..4)
        .map(|i| file_row_data(&format!("r{i}"), &format!("row-{i:02}"), false, i < 3))
        .collect();
    ui.set_file_list(ModelRc::new(VecModel::from(rows)));
    ui.set_selected_count(3);
    settle();

    // 图标档是否真的生效：同一颗按钮在图标档换成 FluentIconButton、标签档是
    // FluentButton。这条是"档位证据"，没有它，下面五条断言在标签档下也能过
    //（标签档的名称来自 text）。
    let narrow_mkdir_icon = count_labelled(&ui, "FluentIconButton", "新建文件夹");
    let narrow_mkdir_text = count_labelled(&ui, "FluentButton", "新建文件夹");
    println!(
        "T1 900x620: 新建文件夹 icon-btn={narrow_mkdir_icon} text-btn={narrow_mkdir_text} \
         （图标档应为 icon=1 / text=0）"
    );
    expect(
        &mut failures,
        narrow_mkdir_icon == 1 && narrow_mkdir_text == 0,
        || {
            format!(
                "T1 900x620: 图标档没有生效 —— 「新建文件夹」实测 icon-btn={narrow_mkdir_icon} \
                 text-btn={narrow_mkdir_text}（期望 1 / 0）。这条是下面五条断言的前提：\
                 900px 窗口（导航折叠 56px → 内容区 844px）必须落进内容区 < 900px 的图标档"
            )
        },
    );

    // 图标档下五个批量按钮的可读名称
    for label in ["复制", "移动到上级", "提取直链", "删除", "取消选择"] {
        let hits = count_labelled(&ui, "FluentButton", label);
        println!("T1 900x620: 按标签 {label:?} 命中 FluentButton {hits} 个");
        expect(&mut failures, hits == 1, || {
            format!(
                "T1 900x620: 按 accessible-label={label:?} 找到 {hits} 个 FluentButton（期望 1）\
                 —— D-01 回退：图标档清空 text 之后没有 tooltip 兜底，可读名称退化成 \"按钮\""
            )
        });
    }

    // ---- 对照档：1180x760（导航展开 248px，内容区 932px ≥ 900px，标签档） ----
    // 标签档下名称来自 `text:`，所以同一批按钮靠 tooltip 与否都读得到；这条对照证明
    // 900px 那条断言的判别力确实来自"图标档 + tooltip"，而不是别的东西。
    let wide = boot(WIDE);
    wide.set_logged_in(true);
    wide.set_file_list(ModelRc::new(VecModel::from(vec![
        file_row_data("w1", "row-00", false, true),
        file_row_data("w2", "row-01", false, true),
        file_row_data("w3", "row-02", false, true),
        file_row_data("w4", "row-03", false, false),
    ])));
    wide.set_selected_count(3);
    settle();
    let wide_mkdir_icon = count_labelled(&wide, "FluentIconButton", "新建文件夹");
    let wide_mkdir_text = count_labelled(&wide, "FluentButton", "新建文件夹");
    println!(
        "T1 1180x760 对照: 新建文件夹 icon-btn={wide_mkdir_icon} text-btn={wide_mkdir_text} \
         （宽档应为 icon=0 / text=1）"
    );
    expect(
        &mut failures,
        wide_mkdir_icon == 0 && wide_mkdir_text == 1,
        || {
            format!(
                "T1 1180x760: 对照档不成立 —— 「新建文件夹」实测 icon-btn={wide_mkdir_icon} \
                 text-btn={wide_mkdir_text}（期望 0 / 1）。窄档断言的判别力依赖这个对照"
            )
        },
    );

    assert!(
        failures.is_empty(),
        "T1 窄档批量栏可读名称失败 {} 项：\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// T2（守 D-02）切到扫码再切回来，已输入的账号不能被清空。
///
/// 缺陷：两个输入框建在 `if !root.qr-mode` 子树里，切扫码时整棵子树被销毁；
/// 若输入值只活在那棵子树里（没有提升到页面状态 `user-text` / `pass-text`），
/// 切回来就是一张空白表单——用户刚打完账号密码，去扫个码回来发现全没了。
///
/// 断言方式：走辅助技术的 `set_accessible_value` 写值（等于"用户输入"那条通道，
/// `lower_accessibility.rs` 把它接到 `TextInput.text` 并触发 `edited`），
/// 切走再切回后重新查一次同一个输入框，值必须还在。
///
/// 中间那条"旧实例必须失效"是关键：它证明输入框**真的被销毁重建过**，
/// 于是这条断言覆盖的正是"值没提升到页面状态"这个缺陷，而不是一个空转的往返。
#[test]
fn check_login_input_survives_mode_switch() {
    let mut failures: Vec<String> = Vec::new();

    let ui = boot(WIDE);
    // 未登录态是默认状态，显式确认一次，避免默认值改动后测试静默失效
    expect(&mut failures, !ui.get_logged_in(), || {
        "T2: 前置条件失败 —— 主窗口不在未登录态".to_string()
    });
    settle();

    let input = text_input_by_placeholder(&ui, "手机号 / 邮箱")
        .expect("T2: 登录页找不到 placeholder 为「手机号 / 邮箱」的 text-input");
    input.set_accessible_value("19051035392");
    settle();
    let typed = input
        .accessible_value()
        .map(|value| value.to_string())
        .unwrap_or_default();
    println!("T2 写入后: accessible-value={typed:?}");
    expect(&mut failures, typed == "19051035392", || {
        format!(
            "T2: 前置条件失败 —— 通过辅助技术写入后 accessible-value 实测 {typed:?}\
             （期望 \"19051035392\"），输入通道本身不通，后面的断言没有意义"
        )
    });

    // 切到扫码：`if !qr-mode` 子树被销毁
    ui.set_qr_mode(true);
    settle();
    println!("T2 切到扫码后: 原输入框实例 is_valid={}", input.is_valid());
    expect(&mut failures, !input.is_valid(), || {
        "T2: 切到扫码登录后原输入框实例仍然存活 —— 断言失去判别力\
         （只有子树真的被销毁重建，值丢失才是这条断言要守的东西）"
            .to_string()
    });

    // 切回密码登录：子树重建，值必须从页面状态回填
    ui.set_qr_mode(false);
    settle();
    let rebuilt = text_input_by_placeholder(&ui, "手机号 / 邮箱")
        .expect("T2: 切回密码登录后登录页没有重建出「手机号 / 邮箱」输入框");
    let survived = rebuilt
        .accessible_value()
        .map(|value| value.to_string())
        .unwrap_or_default();
    println!("T2 切回密码登录后: accessible-value={survived:?}");
    expect(&mut failures, survived == "19051035392", || {
        format!(
            "T2: D-02 回退 —— 扫码往返之后输入框的值实测 {survived:?}（期望 \"19051035392\"）。\
             输入框随 `if !qr-mode` 子树销毁重建时，值必须来自页面状态 user-text"
        )
    });

    assert!(
        failures.is_empty(),
        "T2 登录输入保留失败 {} 项：\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// T3（守 D-03）Toast 必须让开状态栏右下角的传输胶囊。
///
/// 缺陷：Toast 原来贴在 `root.height - 自身高度 - 16px`，正好盖住状态栏右边可点的
/// 传输胶囊（`cloud.slint` 的 `task-chip`，点开传输抽屉）。Toast 一挂 3–8 秒，
/// 这段时间胶囊点不到。
///
/// 回退什么会红：把 `main.slint` 里 Toast 的
/// `- (root.logged-in ? Theme.statusbar-height + Theme.space-2 : 0px)` 删掉 →
/// Toast 下边缘回到 744px，而胶囊顶端在 726px 附近 → 断言立刻变红。
#[test]
fn check_toast_clears_status_bar() {
    let mut failures: Vec<String> = Vec::new();

    let ui = boot(WIDE);
    ui.set_logged_in(true);
    ui.set_status_text(SharedString::from("8 个项目"));
    settle();

    let chip = by_id(&ui, "CloudPage::task-chip");
    let chip_top = chip.absolute_position().y;

    ui.set_notice_text(SharedString::from("已删除 3 项"));
    ui.set_notice_kind(SharedString::from("success"));
    // Toast 的 y 有 240ms 的上滑：小步推进 + 每步读几何，直到收敛
    let toast_bottom = settle_until(|| bottom(&by_id(&ui, "MainWindow::toast")));
    let toast = by_id(&ui, "MainWindow::toast");
    let toast_size = toast.size();

    println!(
        "T3 1180x760: toast=({:.1},{:.1}) {:.1}x{:.1} bottom={toast_bottom:.1} \
         chip.top={chip_top:.1} chip=({:.1},{:.1}) {:.1}x{:.1}",
        toast.absolute_position().x,
        toast.absolute_position().y,
        toast_size.width,
        toast_size.height,
        chip.absolute_position().x,
        chip.absolute_position().y,
        chip.size().width,
        chip.size().height,
    );

    // 先挡掉"Toast 没实例化/零高度"这类假绿：零高度会让下面那条恒真
    expect(
        &mut failures,
        toast_size.height > 1.0 && toast_size.width > 1.0,
        || {
            format!(
                "T3: Toast 尺寸异常 {:.1}x{:.1} —— 断言前置条件不成立",
                toast_size.width, toast_size.height
            )
        },
    );
    expect(&mut failures, toast_bottom <= chip_top + 0.5, || {
        format!(
            "T3: D-03 回退 —— Toast 下边缘 {toast_bottom:.1} 压过传输胶囊顶端 {chip_top:.1}，\
             胶囊被盖住 {:.1}px",
            toast_bottom - chip_top
        )
    });

    assert!(
        failures.is_empty(),
        "T3 Toast 让位状态栏失败 {} 项：\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// T4（守 A-03）模态对话框必须形成焦点闭包：Tab 不能把焦点放回背景。
///
/// 缺陷：Slint 的 `FocusScope` 不拦 Tab，`window.rs` 在按键走完 item 链之后照样
/// `move_focus()` 沿整棵 item 树找下一个可聚焦元素。对话框打开时背景控件没有退出
/// Tab 序列，Tab ×5 会把焦点送到左侧导航项，再按 Space 就**切了页面**——
/// 用户在"确认删除"的对话框上按空格，背后悄悄换到了传输任务页。
///
/// A-03 这一层是**两道互相独立的防线**，断言也分成两截，各自守一道：
///  1. `ModalsApi.blocking`（`main.slint` 的唯一赋值点 → `ActivateScope.enabled`）：
///     浮层在场时背景控件整体退出 Tab 序列。判据是全局量的取值——浮层开合时它必须
///     跟着翻转，且"三种浮层任意一个在场即为真"。把它改成恒 `false`，这一截立刻变红。
///  2. 对话框自己的 `@keys(Tab)` / `@keys(Shift+Tab)` / `@keys(Backtab)` 循环：
///     焦点在对话框内部轮转。判据是 Tab×5 + Space 之后页面与对话框状态不变——
///     删掉那三条 KeyBinding（且没有 blocking 兜底时）焦点会溜到导航项上，
///     实测 `current-page` 从 0 变成 2，这一截变红。
///
/// 为什么两截都要：实测两道防线**各自都够用**（只去掉任何一道，Tab×5 都被关在
/// 对话框里），所以单靠行为断言抓不到"只回退一道"的情形。第一截把 blocking 的
/// 取值本身钉住，第二截把"焦点真的没跑出去"钉住；两条一起才等于 A-03 没被回退。
#[test]
fn check_dialog_focus_closure() {
    let mut failures: Vec<String> = Vec::new();

    let ui = boot(WIDE);
    ui.set_logged_in(true);
    ui.set_current_page(0);
    // 背景被激活的证据：`switch-space` 只有左边导航项会发（切页本身也不发回调）
    let hits: Hits = Rc::new(RefCell::new(Vec::new()));
    let h = hits.clone();
    ui.on_switch_space(move |space| h.borrow_mut().push(format!("switch-space:{space}")));
    settle();

    // ---- 第一截：浮层在场期间 ModalsApi.blocking 必须为真 ----
    let modals = ui.global::<crate::app::ModalsApi>();
    let blocking_before = modals.get_blocking();
    ui.set_dialog_open(true);
    settle();
    let blocking_open = modals.get_blocking();
    ui.set_dialog_open(false);
    settle();
    let blocking_closed = modals.get_blocking();
    ui.set_context_open(true);
    settle();
    let blocking_ctx = modals.get_blocking();
    ui.set_context_open(false);
    settle();
    println!(
        "T4 ModalsApi.blocking: 无浮层={blocking_before} 对话框开={blocking_open} \
         对话框关={blocking_closed} 右键菜单开={blocking_ctx}（期望 false/true/false/true）"
    );
    expect(
        &mut failures,
        !blocking_before && blocking_open && !blocking_closed && blocking_ctx,
        || {
            format!(
                "T4: A-03 回退 —— ModalsApi.blocking 实测 无浮层={blocking_before} \
                 对话框开={blocking_open} 对话框关={blocking_closed} 右键菜单开={blocking_ctx}\
                 （期望 false/true/false/true）。这个全局量是背景控件退出 Tab 序列的唯一开关，\
                 写成恒 false 就没人挡得住 Tab"
            )
        },
    );

    // ---- 第二截：行为面 —— Tab×5 + Space 不能动背景 ----
    ui.set_dialog_title(SharedString::from("确认删除 3 个项目"));
    ui.set_dialog_message(SharedString::from(
        "确定要删除选中的 3 个项目吗？此操作无法恢复。",
    ));
    ui.set_dialog_confirm_text(SharedString::from("删除"));
    ui.set_dialog_danger(true);
    ui.set_dialog_open(true);
    settle();

    for step in 0..5 {
        press_key(&ui, '\u{0009}');
        println!(
            "T4 Tab #{step}: current-page={} dialog-open={}",
            ui.get_current_page(),
            ui.get_dialog_open()
        );
    }
    press_key(&ui, '\u{0020}');

    let page = ui.get_current_page();
    let open = ui.get_dialog_open();
    let fired = hit_list(&hits);
    println!("T4 Tab×5 + Space 之后: current-page={page} dialog-open={open} 回调={fired:?}");

    expect(&mut failures, fired.is_empty(), || {
        format!(
            "T4: A-03 回退 —— 对话框打开期间背景导航项被激活（回调 {fired:?}）\
             —— 焦点从对话框里溜出去了"
        )
    });
    expect(&mut failures, page == 0, || {
        format!(
            "T4: A-03 回退 —— Tab×5 + Space 之后 current-page 变成 {page}（期望 0）\
             —— 焦点落到了背景导航项上，空格把它激活了"
        )
    });
    expect(&mut failures, open, || {
        "T4: A-03 回退 —— 空格把对话框关掉了（dialog-open 变 false）：\
         焦点没有留在对话框内部"
            .to_string()
    });

    assert!(
        failures.is_empty(),
        "T4 对话框焦点闭包失败 {} 项：\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// T5（守 A-01）`selection-index` 变化必须把那一行滚进视口。
///
/// 缺陷：键盘选行只改选择集，ListView 的视口不动，于是"选中了第 31 行但屏幕上
/// 还是前十几行"——看起来什么都没发生。修法是在 `changed selection-index` 里
/// 按行高把 `viewport-y` 补到目标行可见。
///
/// 断言落在**可观测输出**上：ListView 是虚拟化的，只实例化视口附近的行，
/// 所以"能不能按 accessible-label 查到 row-30"直接等价于"它有没有被滚进视口"。
/// 刻意不读 `viewport-y` 再按行高反算区间——那是把实现抄一遍，实现一改断言就废。
#[test]
fn check_selection_index_brings_row_into_view() {
    let mut failures: Vec<String> = Vec::new();

    let ui = boot(WIDE);
    ui.set_logged_in(true);
    ui.set_current_page(0);
    let rows: Vec<crate::app::FileEntry> = (0..40)
        .map(|i| file_row_data(&format!("r{i:02}"), &format!("row-{i:02}"), false, false))
        .collect();
    ui.set_file_list(ModelRc::new(VecModel::from(rows)));
    ui.set_selection_index(-1);
    ui.set_selected_count(0);
    settle();

    let before = instantiated_row_labels(&ui);
    println!(
        "T5 selection-index=-1: 已实例化 {} 行 {:?}…{:?}",
        before.len(),
        before.first(),
        before.last()
    );
    expect(
        &mut failures,
        !before.contains(&"row-30".to_string()),
        || {
            "T5: 前置条件失败 —— 无选中时 row-30 就已经在元素树里，\
         这条断言没有判别力（视口一屏装下了全部 40 行？）"
                .to_string()
        },
    );

    ui.set_selection_index(30);
    settle();
    let after = instantiated_row_labels(&ui);
    println!(
        "T5 selection-index=30: 已实例化 {} 行 {:?}…{:?}",
        after.len(),
        after.first(),
        after.last()
    );
    expect(&mut failures, after.contains(&"row-30".to_string()), || {
        format!(
            "T5: A-01 回退 —— selection-index=30 之后 row-30 仍未被实例化，\
             视口没有跟随选择。当前可见行 {:?}",
            after
        )
    });

    assert!(
        failures.is_empty(),
        "T5 选中行进视口失败 {} 项：\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// T6（守 D-06）切页必须收掉 Tooltip 气泡。
///
/// 缺陷：气泡是窗口级浮层，指针悬停在行内图标按钮上时点亮。切页后那些按钮随页面
/// 一起隐藏，**不会**产生 `has-hover = false`，气泡于是原地常驻、指着一个已经不存在
/// 的控件。修法是在 `changed current-page` 里清掉 `TooltipState.visible`。
///
/// 回退什么会红：删掉 `main.slint` 里 `changed current-page` 中的
/// `TooltipState.visible = false;` 那一行。
#[test]
fn check_tooltip_clears_on_page_switch() {
    let mut failures: Vec<String> = Vec::new();

    let ui = boot(WIDE);
    ui.set_logged_in(true);
    settle();

    let tooltip = ui.global::<crate::app::TooltipState>();
    tooltip.set_text(SharedString::from("刷新 (F5)"));
    tooltip.set_visible(true);
    settle();
    let lit = tooltip.get_visible();
    println!("T6 点亮后: TooltipState.visible={lit}");
    expect(&mut failures, lit, || {
        "T6: 前置条件失败 —— TooltipState.visible 写不进 true".to_string()
    });

    ui.set_current_page(2);
    settle();
    let after = tooltip.get_visible();
    println!("T6 切页后: TooltipState.visible={after}");
    expect(&mut failures, !after, || {
        "T6: D-06 回退 —— 切页之后 TooltipState.visible 仍是 true，\
         气泡会挂在一个已经不存在的控件旁边"
            .to_string()
    });

    assert!(
        failures.is_empty(),
        "T6 切页收 Tooltip 失败 {} 项：\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// T7（守 A-01 的按键链）方向键必须把选择移进第一行、再移到第二行。
///
/// 缺陷：这张键盘表原来只有"对已选中项做动作"的键（Enter / F2 / Delete…），
/// 没有任何"移动选择"的键，纯键盘用户根本定不出第一行，只能用鼠标点。
///
/// 断言拆成两截：
///   · `move-selection` 回调收到 `(1, false)` —— 证明 `@keys(DownArrow)` 这条
///     KeyBinding 真的被派发到了（回退成"删掉这条 KeyBinding"，回调一次都不响）；
///   · `selection-index` / `selected-count` 随之推进 —— 证明回调的结果被写回了 UI。
///
/// 控制器真实的 `handle_move_selection` 需要 `Arc<Controller>`（要后端），
/// 这里用最小替身接住回调：本断言守的是 **UI 侧的按键链**，选择集算法本身由
/// `controller/files.rs` 的 `move_selection_tests` 覆盖。
#[test]
fn check_arrow_key_moves_selection() {
    let mut failures: Vec<String> = Vec::new();

    let ui = boot(WIDE);
    ui.set_logged_in(true);
    ui.set_current_page(0);
    ui.set_file_list(ModelRc::new(VecModel::from(vec![
        file_row_data("r0", "row-00", false, false),
        file_row_data("r1", "row-01", false, false),
        file_row_data("r2", "row-02", false, false),
    ])));
    ui.set_selected_count(0);
    ui.set_selection_index(-1);
    settle();

    let moves: Rc<RefCell<Vec<(i32, bool)>>> = Rc::new(RefCell::new(Vec::new()));
    let cursor = Rc::new(Cell::new(-1_i32));
    {
        let weak = ui.as_weak();
        let moves = moves.clone();
        let cursor = cursor.clone();
        ui.on_move_selection(move |delta, extend| {
            moves.borrow_mut().push((delta, extend));
            let from = if cursor.get() < 0 {
                0
            } else {
                cursor.get() + delta
            };
            let to = from.clamp(0, 2);
            cursor.set(to);
            if let Some(ui) = weak.upgrade() {
                ui.set_selection_index(to);
                ui.set_selected_count(1);
            }
        });
    }

    // 让焦点落进文件列表：单击一行会走 `item-clicked` → `list-area.focus()`。
    // 未接 select-item 回调，所以这一下点击不会改选择集。
    first_row(&ui).mock_single_click(PointerEventButton::Left);
    settle();
    println!(
        "T7 点击后（应仍未选中）: selection-index={} selected-count={}",
        ui.get_selection_index(),
        ui.get_selected_count()
    );
    expect(
        &mut failures,
        ui.get_selection_index() == -1 && ui.get_selected_count() == 0,
        || {
            format!(
                "T7: 前置条件失败 —— 点击行之后选择状态已被改动 \
                 (index={}, count={})，无法验证\"第一下方向键落到第一行\"",
                ui.get_selection_index(),
                ui.get_selected_count()
            )
        },
    );

    press_key(&ui, '\u{F701}');
    let first = (
        ui.get_selection_index(),
        ui.get_selected_count(),
        moves.borrow().clone(),
    );
    println!(
        "T7 第一次 DownArrow: selection-index={} selected-count={} move-selection={:?}",
        first.0, first.1, first.2
    );
    expect(&mut failures, first.2 == vec![(1, false)], || {
        format!(
            "T7: A-01 回退 —— 第一下 DownArrow 派发出的 move-selection 实测 {:?}\
             （期望 [(1, false)]）—— 文件列表上的 @keys(DownArrow) 绑定丢了，\
             或焦点不在列表上",
            first.2
        )
    });
    expect(&mut failures, first.0 == 0 && first.1 == 1, || {
        format!(
            "T7: 第一下 DownArrow 之后 selection-index/selected-count 实测 {}/{}（期望 0/1）",
            first.0, first.1
        )
    });

    press_key(&ui, '\u{F701}');
    let second = (
        ui.get_selection_index(),
        ui.get_selected_count(),
        moves.borrow().clone(),
    );
    println!(
        "T7 第二次 DownArrow: selection-index={} selected-count={} move-selection={:?}",
        second.0, second.1, second.2
    );
    expect(
        &mut failures,
        second.2 == vec![(1, false), (1, false)],
        || {
            format!(
                "T7: 第二下 DownArrow 派发出的 move-selection 实测 {:?}（期望两次 (1, false)）",
                second.2
            )
        },
    );
    expect(&mut failures, second.0 == 1, || {
        format!(
            "T7: 第二下 DownArrow 之后 selection-index 实测 {}（期望 1）",
            second.0
        )
    });

    assert!(
        failures.is_empty(),
        "T7 方向键选择失败 {} 项：\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// 从元素树反推当前的 `detail-level`
///
/// `detail-level` 是 `ui/main.slint` 的本地属性，Rust 侧没有 getter；而它驱动的是
/// 三个条件渲染的列头（`if root.detail-level >= n`），所以「列头在不在元素树里」
/// 就是它的可观测等价物。这条反推在 T8 的扫掠里被复用。
fn detail_level_of(ui: &crate::app::MainWindow) -> i32 {
    let present = |id: &str| {
        ElementQuery::from_root(ui)
            .match_id(id)
            .find_first()
            .is_some()
    };
    if present("CloudPage::kind-header") {
        3
    } else if present("CloudPage::time-header") {
        2
    } else if present("CloudPage::size-header") {
        1
    } else {
        0
    }
}

/// T8（守 D-11）`(导航档, detail-level, 批量栏档)` 必须关于整窗宽度单调不减。
///
/// 缺陷（D-11）：两条断点不同源——导航栏按「整窗 < 960px」折叠，批量栏按「内容区
/// < 760px」退化成图标档。整窗 959px 时导航折叠（内容区 903px，批量栏全标签）；
/// 整窗 960px 时导航展开吃掉 248px（内容区 712px，批量栏退化成图标档、Ctrl+单击
/// 提示与状态栏容量文字一起消失）。窗口变宽 1px，能力反而少一截。
///
/// 修法：两条断点都由 `LayoutMetrics.min-capability-width`（900px）推导，于是整窗
/// 走到 1148px（= 248 + 900）导航才展开，展开的瞬间内容区正好还是 900px——跨过
/// 断点时能力不降。
///
/// 采样策略：900..1000 步长 1（这一段的断点最密，956px 是批量栏回到标签档的那一档），
/// 1000..1600 步长 10（这一段只有 1148 导航、1548 类型列两个断点，10px 的粒度足够
/// 判定"跨过断点之后不再回退"），另外显式补进 1147 / 1148 / 1368 三档——前两个是
/// 导航断点的两侧，必须逐像素比对；1368 是「修改时间」列打开的那一档。
/// 全程复用**同一个**窗口实例（只改尺寸）：比每档重建窗口便宜，也更贴合"同一个用户
/// 把窗口从最小拖到最大"这件事。
///
/// 回退什么会红：把 `pages/cloud.slint` 的 `narrow` 改回 `root.width < 760px`（旧断点）
/// → 956–1007px 区间里 detail-level 与批量栏档随导航折叠一起塌下去，单调性立刻破。
#[test]
fn check_capability_monotonic_in_width() {
    let mut failures: Vec<String> = Vec::new();

    let ui = boot((900.0, 760.0));
    ui.set_logged_in(true);
    ui.set_current_page(0);
    settle();

    let mut widths: Vec<u32> = (900..=1000).collect();
    widths.extend((1010..=1600).step_by(10));
    widths.extend([1147, 1148, 1368]);
    widths.sort_unstable();
    widths.dedup();

    // (整窗宽, 导航是否展开, detail-level, 是否图标档)
    let mut samples: Vec<(u32, bool, i32, bool)> = Vec::new();

    for width in widths {
        ui.window()
            .set_size(slint::LogicalSize::new(width as f32, 760.0));
        // 导航栏的宽度有 180ms 过渡（`animate width`）：按 `check_drawer_reveal`
        // 同一套纪律"小步推进 + 每步读一次几何"采样到收敛，只推时间不读是推不动的。
        let nav = settle_until(|| by_id(&ui, "MainWindow::nav").size().width);
        settle();

        let (expanded, compact) = nav_widths(&ui);
        let nav_expanded = if (nav - expanded).abs() <= 1.0 {
            true
        } else if (nav - compact).abs() <= 1.0 {
            false
        } else {
            failures.push(format!(
                "T8 {width}px: 导航栏宽度实测 {nav:.1}，既不是展开档 {expanded:.0} \
                 也不是折叠档 {compact:.0}"
            ));
            false
        };

        let level = detail_level_of(&ui);
        let icon_bar = count_labelled(&ui, "FluentIconButton", "新建文件夹") == 1;
        samples.push((width, nav_expanded, level, icon_bar));

        println!(
            "T8 {width}px: 导航 {nav:.0}（{}）/ 内容区 {:.0} / detail-level={level} / 批量栏={}",
            if nav_expanded { "展开" } else { "折叠" },
            width as f32 - nav,
            if icon_bar { "图标档" } else { "标签档" },
        );
    }

    expect(&mut failures, !samples.is_empty(), || {
        "T8: 扫掠没有采到任何样本".to_string()
    });

    // ---- 单调不减：窗口变宽，任何一项能力都不许倒退 ----
    for pair in samples.windows(2) {
        let (w0, nav0, level0, icon0) = pair[0];
        let (w1, nav1, level1, icon1) = pair[1];
        if nav0 && !nav1 {
            failures.push(format!(
                "T8: 导航档在 {w1}px 回退（{w0}px 是展开、{w1}px 变成折叠）—— \
                 窗口变宽，导航反而收起"
            ));
        }
        if level1 < level0 {
            failures.push(format!(
                "T8: detail-level 在 {w1}px 回退（{w0}px 是 {level0} 档、{w1}px 是 {level1} 档）\
                 —— 窗口变宽，列反而变少"
            ));
        }
        if !icon0 && icon1 {
            failures.push(format!(
                "T8: 批量栏档在 {w1}px 回退（{w0}px 是标签档、{w1}px 变成图标档）—— \
                 窗口变宽，批量栏反而只剩图标"
            ));
        }
    }

    // ---- 导航断点两侧必须同档（D-11 的反面） ----
    //
    // 注意"同档"指的是**能力对** `(detail-level, 批量栏档)`：导航宽度本身在
    // 1147/1148 之间必然不同，那正是断点本身；要钉住的是"跨过断点不丢能力"。
    let at = |w: u32| samples.iter().find(|sample| sample.0 == w).copied();
    match (at(1147), at(1148)) {
        (Some(a), Some(b)) => {
            expect(&mut failures, a.1 != b.1, || {
                format!(
                    "T8: 1147px 与 1148px 的导航档实测同为{}——导航断点没有落在 \
                     Theme.nav-width + min-capability-width 上，下面那条同档断言是空转的",
                    if a.1 { "展开" } else { "折叠" }
                )
            });
            expect(&mut failures, a.2 == b.2 && a.3 == b.3, || {
                format!(
                    "T8: 1147px 与 1148px 跨导航断点后能力变了 —— 1147px 是 \
                     (detail-level={}, 图标档={})，1148px 是 (detail-level={}, 图标档={})。\
                     这正是 D-11 的形状：窗口变宽 1px，能力掉一截",
                    a.2, a.3, b.2, b.3,
                )
            });
        }
        _ => failures.push(
            "T8: 扫掠里没有同时采到 1147px 与 1148px，断点两侧的同档断言无从判定".to_string(),
        ),
    }

    assert!(
        failures.is_empty(),
        "T8 断点单调性失败 {} 项：\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// T9（守 D-12）排序入口必须在**任何**窗口宽度下都可达，且浮层真的能派发排序。
///
/// 缺陷（D-12）：`sort-changed` 原来只有三个调用点，全在列头上；而「修改时间」列头在
/// `if root.detail-level >= 2` 里（内容区 ≥ 1120px，整窗 ≥ 1368px 且导航展开）。
/// 默认 1180px 窗口的内容区只有 932px → detail-level=1 → 用户**没有任何入口**按
/// 修改时间排序。修法是在命令栏加一个与窗口宽度无关的排序入口（下拉浮层）。
///
/// 断言覆盖的链路完整：按可读名称找到「排序」按钮 → 触发（测试后端不能发鼠标事件，
/// 用辅助技术的 `invoke_accessible_default_action()`，与 `check_a11y_semantics` 同一
/// 手法）→ 浮层里按可读名称查到「修改时间」项 → 触发它 → 组件回调 `sort-changed`
/// 收到 2。顺带钉住浮层与 `ModalsApi.blocking` 的联动（浮层开着时背景控件必须退出
/// Tab 序列）。
#[test]
fn check_sort_menu_reachable_at_all_widths() {
    let mut failures: Vec<String> = Vec::new();

    // 900x620 = 最小窗口（图标档批量栏）；1600x760 = 四列全开（列头入口也在）
    for (width, height) in [NARROW, EXTRA_WIDE] {
        let ui = boot((width, height));
        ui.set_logged_in(true);
        ui.set_current_page(0);
        settle();

        // 前置条件：窄档下「修改时间」列头不在元素树里——那正是 D-12 的根因，
        // 说明这条窗口宽度下命令栏入口是**唯一**的按时间排序通道。
        let header = ElementQuery::from_root(&ui)
            .match_id("CloudPage::time-header")
            .find_first();
        println!(
            "T9 {width}x{height}: 「修改时间」列头在不在元素树里={}",
            header.is_some()
        );

        let fired: Rc<RefCell<Vec<i32>>> = Rc::new(RefCell::new(Vec::new()));
        let h = fired.clone();
        ui.on_sort_changed(move |key| h.borrow_mut().push(key));

        let modals = ui.global::<crate::app::ModalsApi>();
        let blocking_before = modals.get_blocking();

        let button = labelled(&ui, "FluentIconButton", "排序");
        let Some(button) = button else {
            failures.push(format!(
                "T9 {width}x{height}: 命令栏里按 accessible-label=\"排序\" 找不到 \
                 FluentIconButton —— 排序入口没有渲染出来"
            ));
            continue;
        };

        button.invoke_accessible_default_action();
        settle();
        let open = ui.get_sort_menu_open();
        let blocking_open = modals.get_blocking();
        println!(
            "T9 {width}x{height}: 点「排序」后 open={open} blocking={blocking_open}\
             （期望 true/true）"
        );
        expect(&mut failures, open, || {
            format!(
                "T9 {width}x{height}: 触发「排序」按钮后 sort-menu-open 仍是 false —— \
                 浮层没打开"
            )
        });
        expect(&mut failures, blocking_open, || {
            format!(
                "T9 {width}x{height}: 排序浮层打开时 ModalsApi.blocking 仍是 false —— \
                 背景控件没有退出 Tab 序列（`main.slint` 的 modals-blocking 漏了 \
                 sort-menu-open）"
            )
        });

        // 浮层是否真的长出来了：按菜单项的可读名称查（FluentMenuItem 的
        // accessible-label 就是 label）。
        let item = labelled(&ui, "FluentMenuItem", "修改时间");
        let Some(item) = item else {
            failures.push(format!(
                "T9 {width}x{height}: 浮层里按 accessible-label=\"修改时间\" 找不到 \
                 FluentMenuItem —— 三条排序项没渲染出来"
            ));
            continue;
        };
        let item_size = item.size();
        println!(
            "T9 {width}x{height}: 「修改时间」项在 ({:.1},{:.1}) {:.1}x{:.1}",
            item.absolute_position().x,
            item.absolute_position().y,
            item_size.width,
            item_size.height,
        );
        expect(&mut failures, item_size.height > 1.0, || {
            format!(
                "T9 {width}x{height}: 「修改时间」项高度 {:.1}，浮层尺寸异常",
                item_size.height
            )
        });

        item.invoke_accessible_default_action();
        settle();
        let keys = fired.borrow().clone();
        let blocking_closed = modals.get_blocking();
        let open_after = ui.get_sort_menu_open();
        println!(
            "T9 {width}x{height}: 选「修改时间」后 sort-changed={keys:?} open={open_after} \
             blocking={blocking_closed}（期望 [2]/false/false）"
        );
        expect(&mut failures, keys == vec![2], || {
            format!(
                "T9 {width}x{height}: D-12 回退 —— 选「修改时间」派发的 sort-changed 实测 \
                 {keys:?}（期望 [2]）：命令栏入口没有接到既有的 `sort-changed` 回调"
            )
        });
        expect(&mut failures, !open_after, || {
            format!("T9 {width}x{height}: 选完之后排序浮层没有收起（sort-menu-open 仍为 true）")
        });
        expect(&mut failures, !blocking_closed, || {
            format!(
                "T9 {width}x{height}: 浮层收起后 ModalsApi.blocking 仍是 true —— \
                 背景控件会永久退出 Tab 序列"
            )
        });
        // 无浮层时本来就不该是 true，这条把 `blocking-before` 钉成基线
        expect(&mut failures, !blocking_before, || {
            format!("T9 {width}x{height}: 还没开浮层时 ModalsApi.blocking 就是 true 了")
        });
    }

    assert!(
        failures.is_empty(),
        "T9 排序入口可达性失败 {} 项：\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// T10（守 D-13）测试侧推算的断点必须与**渲染出来的**导航栏一致。
///
/// D-13 的形状是"同一个断点存在两份常量"：`ui/` 里一份真值，`ui_tests.rs` 里一份副本
/// （原来是 `NAV_WIDTH = 248.0` / `NAV_WIDTH_COMPACT = 56.0` /
/// `NAV_COMPACT_BREAKPOINT = 960.0`）。副本最坏的后果不是报错，而是**静默失真**：
/// UI 改了断点，测试仍按旧数字推算内容区，断言全跑在错误的坐标上却照样"通过"。
///
/// 这条断言把两侧钉在一起：`content_width()`（全部操作数取自 `Theme` 与
/// `LayoutMetrics`）推算出的导航宽度，必须等于导航栏元素实测的宽度。任何一处重新长出
/// 硬编码副本，跨过旧断点的那一段就会立刻对不上——960–1147px 正是 D-11 出事的那一段。
#[test]
fn check_breakpoint_tokens_match_rendered_nav() {
    let mut failures: Vec<String> = Vec::new();

    let ui = boot((900.0, 760.0));
    ui.set_logged_in(true);
    settle();

    let (expanded, compact) = nav_widths(&ui);
    let breakpoint = nav_breakpoint(&ui);
    println!(
        "T10: Theme.nav-width={expanded:.0} nav-width-compact={compact:.0} \
         LayoutMetrics.min-capability-width={:.0} → 导航断点 {breakpoint:.0}",
        breakpoint - expanded,
    );

    // 前置条件：断点必须落在采样区间（900..1600）里面，否则两侧比对是空转的
    expect(
        &mut failures,
        breakpoint > 900.0 && breakpoint < 1600.0,
        || {
            format!(
                "T10: 导航断点 {breakpoint:.0} 落在采样区间 900..1600 之外，\
                 下面两侧的比对无从判定"
            )
        },
    );

    // 采样点：断点两侧各一档 + D-11 出事的那一段（960/1100）+ 三个历史档位
    let mut widths = vec![
        900.0_f32,
        960.0,
        1000.0,
        1100.0,
        breakpoint - 1.0,
        breakpoint,
        1180.0,
        1368.0,
        1600.0,
    ];
    widths.sort_by(f32::total_cmp);
    widths.dedup_by(|a, b| (*a - *b).abs() < 0.5);

    for width in widths {
        ui.window().set_size(slint::LogicalSize::new(width, 760.0));
        let nav = settle_until(|| by_id(&ui, "MainWindow::nav").size().width);
        let from_helper = width - content_width(&ui, width);
        let expected = if width < breakpoint {
            compact
        } else {
            expanded
        };

        println!(
            "T10 {width:.0}px: 导航实测 {nav:.0}（按令牌应为 {expected:.0}）\
             / content_width 推算 {from_helper:.0}"
        );

        expect(&mut failures, (nav - expected).abs() <= 1.0, || {
            format!(
                "T10 {width:.0}px: 导航栏实测宽度 {nav:.0} 与令牌算出的 {expected:.0} 不符 —— \
                 `ui/main.slint` 的 `nav-compact` 已经不再等于 \
                 `Theme.nav-width + LayoutMetrics.min-capability-width`"
            )
        });
        expect(&mut failures, (from_helper - nav).abs() <= 1.0, || {
            format!(
                "T10 {width:.0}px: 测试侧 `content_width()` 推算的导航宽度 {from_helper:.0} \
                 与导航栏实测 {nav:.0} 不一致 —— 断点常量又长出了第二份副本（D-13）"
            )
        });
    }

    assert!(
        failures.is_empty(),
        "T10 断点单一来源失败 {} 项：\n{}",
        failures.len(),
        failures.join("\n")
    );
}

// ==================== H. 运行时切语（设置页"无需重启"的技术前提） ====================//
// 切语靠的是 `slint::select_bundled_translation`：它置脏之后，所有 `@tr` 绑定（含
// `accessible-label` 上的）在下一帧重算。`crate::i18n::apply` 内部做的正是这一次调用。
//
// 这里刻意直接打底层接口、而不是走 `i18n::apply`：`i18n::CURRENT` 是**进程级** atomic，
// 改它会和并行跑的其它 `#[test]` 抢（那些断言要查 Rust 侧中文文案）；而
// `select_bundled_translation` 只作用于本线程的 `GLOBAL_CONTEXT`，天然隔离。
//
// 这条同时是 `boot()` 那句"钉简中"的守门员：那句的注释解释了 build.rs 打包语言包之后
// Slint 会按系统 locale 自动选包（本机 zh-CN 的主标签命中 zh-Hant），这里从另一头验证
// "选哪个包 → 界面就是哪个语种"，两边合起来才算把这件事钉住。
#[test]
fn check_language_switch_retranslates_ui() {
    let mut failures: Vec<String> = Vec::new();

    let ui = boot(WIDE);
    ui.set_logged_in(true);
    ui.set_current_page(4);
    settle();

    let zh = a11y_hits(&ui, "减少动画");
    println!("H1 简中：按「减少动画」命中 {} 个元素", zh.len());
    expect(&mut failures, !zh.is_empty(), || {
        "H1: 简中下按 accessible-label=\"减少动画\" 一个元素都找不到 —— \
         boot() 的钉简中失效了"
            .to_string()
    });

    // 切英文：同一棵**已经建好**的组件树上的 @tr 必须当场重算
    let switched = slint::select_bundled_translation("en");
    let ok = switched.is_ok();
    let why = format!("{:?}", switched.err());
    expect(&mut failures, ok, || format!("H2: 切到 en 失败：{why}"));
    settle();

    let en = a11y_hits(&ui, "Reduce motion");
    let stale = a11y_hits(&ui, "减少动画");
    println!(
        "H2 英文：\"Reduce motion\" 命中 {} 个元素，残留的「减少动画」{} 个",
        en.len(),
        stale.len()
    );
    expect(&mut failures, !en.is_empty(), || {
        format!(
            "H2: 切到英文后按 \"Reduce motion\" 命中 {} 个元素（期望 ≥1）—— \
             select_bundled_translation 之后 @tr 绑定没有重算",
            en.len()
        )
    });
    expect(&mut failures, stale.is_empty(), || {
        format!(
            "H3: 切到英文后仍能按「减少动画」命中 {} 个元素 —— 旧语种文案没被替换",
            stale.len()
        )
    });

    // 复原本线程的语种：本文件里 boot() 会被反复调用，留成英文会让后面的断言拿到错语种
    let _ = slint::select_bundled_translation("");

    assert!(
        failures.is_empty(),
        "运行时切语失败 {} 项：\n{}",
        failures.len(),
        failures.join("\n")
    );
}
