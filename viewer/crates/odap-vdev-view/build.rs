fn main() {
    // The window is dark: Slint's own widgets (the slider) in their dark style.
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/view.slint", config).unwrap();
}
