fn main() {
    // 组件与主题拆分为多个文件，任何 .slint 变化都需要重新编译
    println!("cargo:rerun-if-changed=ui");
    // 译文目录（编译期打包的语言包）与 Rust 侧词表：改了要重跑 build script，
    // 否则新加的 @tr 文案会拿着上一次的 .po 继续编。
    println!("cargo:rerun-if-changed=lang");
    println!("cargo:rerun-if-changed=i18n");
    let config = || {
        slint_build::CompilerConfiguration::new()
            .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles)
            // 元素 id / 类型名表：src/ui_tests.rs 的几何断言靠它定位元素
            // （i-slint-backend-testing 的 ElementQuery API 在缺这份数据时会静默
            // 丢掉元素 id）。只在 dev/test 构建里生成，release 二进制不带这份调试数据。
            .with_debug_info(cfg!(debug_assertions))
            // 关闭「默认上下文=组件名」：全库共用一张 msgid 表，.po 里不写 msgctxt。
            // 否则同一句文案在 main.slint 与各组件里各算一条 msgid，翻译得重复填。
            .with_default_translation_context(slint_build::DefaultTranslationContext::None)
            // 编译期打包 lang/<lang>/LC_MESSAGES/tianyi-app.po，运行时
            // slint::select_bundled_translation() 切语（索引 0 = @tr 原文 = 简中）。
            .with_bundled_translations("lang")
    };
    slint_build::compile_with_config("ui/main.slint", config()).expect("slint compile");

    // 图标总览：只服务 examples/icon_sheet.rs 的视觉自检，
    // 单独编译到 OUT_DIR，不进入主界面的组件树。
    // 该文件近 6000 行，常规构建靠 feature 门控整体跳过，只有显式
    // `--features icon-sheet` 时才编译（example 侧由 required-features 绑定）。
    if cfg!(feature = "icon-sheet") {
        let sheet_out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"))
            .join("icon_sheet.rs");
        slint_build::compile_with_output_path("ui/dev/icon-sheet.slint", sheet_out, config())
            .expect("slint compile icon-sheet");
    }
}
