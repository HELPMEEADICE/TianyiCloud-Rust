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

use i_slint_backend_testing::{AccessibleRole, ElementHandle, ElementQuery};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::rc::Rc;
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

/// 状态栏传输胶囊的内容宽度上界（px）：实际内容宽约 84px，这里给字体度量留余量，
/// 同时与"被主轴 stretch 拉宽"后的量级（400px 以上）保持可区分的差距
const CHIP_MAX_WIDTH: f32 = 120.0;

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

/// 导航栏宽度（`ui/main.slint` 的 `nav-compact` 断点与 `Theme.nav-width*` 令牌）
const NAV_WIDTH: f32 = 248.0;
/// 导航栏折叠后的宽度（窗口窄于 960px 时折叠）
const NAV_WIDTH_COMPACT: f32 = 56.0;
/// `ui/main.slint` 的折叠断点
const NAV_COMPACT_BREAKPOINT: f32 = 960.0;
/// DE-07 用的宽档：内容区宽度 1352px，四列全开
const EXTRA_WIDE: (f32, f32) = (1600.0, 760.0);

/// 内容区可用宽度 = 整窗宽度 − 导航栏宽度
fn content_width(window_width: f32) -> f32 {
    let nav = if window_width < NAV_COMPACT_BREAKPOINT {
        NAV_WIDTH_COMPACT
    } else {
        NAV_WIDTH
    };
    window_width - nav
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
        let content = content_width(width);

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
