fn main() {
    // 组件与主题拆分为多个文件，任何 .slint 变化都需要重新编译
    println!("cargo:rerun-if-changed=ui");
    let config = || {
        slint_build::CompilerConfiguration::new()
            .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles)
    };
    slint_build::compile_with_config("ui/main.slint", config()).expect("slint compile");

    // 图标总览：只服务 examples/icon_sheet.rs 的视觉自检，
    // 单独编译到 OUT_DIR，不进入主界面的组件树。
    let sheet_out =
        std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("icon_sheet.rs");
    slint_build::compile_with_output_path("ui/dev/icon-sheet.slint", sheet_out, config())
        .expect("slint compile icon-sheet");
}
