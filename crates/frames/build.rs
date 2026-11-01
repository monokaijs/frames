fn main() {
    println!("cargo:rerun-if-env-changed=FRAMES_RELEASE_BUILD");
    println!("cargo:rerun-if-changed=../../assets/icons");
    println!("cargo:rerun-if-changed=../../native/windows/frames.manifest");
    #[cfg(windows)]
    {
        winresource::WindowsResource::new()
            .set_manifest_file("../../native/windows/frames.manifest")
            .set_icon("../../assets/icons/Frames.ico")
            .set("ProductName", "Frames")
            .set("FileDescription", "Frames")
            .set("InternalName", "Frames")
            .set("OriginalFilename", "Frames.exe")
            .compile()
            .expect("compile Frames application icon and metadata");
    }
    slint_build::compile_with_config(
        "ui/launcher.slint",
        slint_build::CompilerConfiguration::new().with_style("fluent-light".into()),
    )
    .expect("compile Frames launcher");
}
