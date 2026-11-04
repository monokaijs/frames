fn main() {
    println!("cargo:rerun-if-env-changed=FRAMES_RELEASE_BUILD");
    // `frames next` links the v2 macOS media library, whose Swift runtime
    // lives in /usr/lib/swift (a dependency's link arguments do not reach
    // this binary).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,/usr/lib/swift");
    }
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
