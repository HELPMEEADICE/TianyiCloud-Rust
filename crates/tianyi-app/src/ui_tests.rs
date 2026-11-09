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
//! 为什么只有一个 `#[test]`：测试后端把 Slint 平台装在**首先初始化它的那个线程**上
//! （"The Slint platform was initialized in another thread"），多个 `#[test]` 并行时
//! 只有第一个能建窗口。因此四条断言放在同一个测试函数里顺序执行，失败信息汇总后
//! 一次性报出 —— 一条红不等于其他三条是绿的。
//!
//! 每条断言各自守住一个历史上真实出现过的布局缺陷，注释里写清了根因，
//! 改 UI 时若把它们改红，先读那条注释再决定是不是真的要改布局。

use i_slint_backend_testing::{ElementHandle, ElementQuery};
use slint::{ComponentHandle, Model};
use std::sync::Once;
use std::time::Duration;

/// 窄档：`ui/main.slint` 允许的最小窗口尺寸
const NARROW: (f32, f32) = (900.0, 620.0);
/// 默认档：`ui/main.slint` 的首选窗口尺寸
const WIDE: (f32, f32) = (1180.0, 760.0);

/// 两档窗口尺寸，`(宽, 高)`
const SIZES: [(f32, f32); 2] = [NARROW, WIDE];

/// 云盘页根布局的横向内边距：`ui/pages/cloud.slint` 的 `padding-left/right: Theme.space-4`
const PAGE_PADDING: f32 = 16.0;

/// 进程内只能初始化一次测试后端（Slint 的平台是进程级全局状态）
static INIT: Once = Once::new();

/// 几何回归总入口：四条断言在同一个线程里顺序跑完，失败信息一次性汇总
#[test]
fn ui_geometry_regression() {
    let mut failures: Vec<String> = Vec::new();
    check_login_card(&mut failures);
    check_task_chip(&mut failures);
    check_context_menu(&mut failures);
    check_empty_state_icon(&mut failures);
    assert!(
        failures.is_empty(),
        "几何回归失败 {} 项：\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// 初始化测试后端，并返回一个尺寸已设定、布局已落定的主窗口
fn boot(size: (f32, f32)) -> crate::app::MainWindow {
    INIT.call_once(i_slint_backend_testing::init_integration_test_with_mock_time);
    let ui = crate::app::MainWindow::new().expect("主窗口构造失败");
    ui.window()
        .set_size(slint::LogicalSize::new(size.0, size.1));
    ui.show().expect("窗口显示失败");
    settle();
    ui
}

/// 推进虚拟时间，让布局与过渡动画落定（不 sleep、不读墙钟）
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

/// B. 状态栏传输胶囊：右边缘贴着内容区右边缘减去页内边距。
///
/// 根因：`Rectangle` 在 `HorizontalLayout` 里默认参与主轴 stretch 分配，
/// 少了 `horizontal-stretch: 0` 就会被拉成整行；这条断言同时守住"被拉宽"和
/// "贴错边"两种表现（右边缘会明显偏离）。
fn check_task_chip(failures: &mut Vec<String>) {
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
        // 胶囊按内容自适应宽度：被主轴 stretch 拉成整行时这个上限会被突破
        expect(failures, chip_size.width < width / 2.0, || {
            format!(
                "B {width}x{height}: 传输胶囊宽度 {:.1} 被拉成了整行",
                chip_size.width,
            )
        });
    }
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
