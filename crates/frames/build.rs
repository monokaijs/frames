fn main() {
    slint_build::compile_with_config(
        "ui/launcher.slint",
        slint_build::CompilerConfiguration::new().with_style("fluent-light".into()),
    )
    .expect("compile Frames launcher");
}
