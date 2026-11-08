fn main() {
    // 组件与主题拆分为多个文件，任何 .slint 变化都需要重新编译
    println!("cargo:rerun-if-changed=ui");
    let config = slint_build::CompilerConfiguration::new()
        .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles);
    slint_build::compile_with_config("ui/main.slint", config).expect("slint compile");
}
