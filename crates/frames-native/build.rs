fn main() {
    println!("cargo:rustc-check-cfg=cfg(frames_shared_input)");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc")
        || std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("x86_64")
    {
        return;
    }
    println!("cargo:rerun-if-changed=../../native/windows/shared-input");
    println!("cargo:rerun-if-changed=../../vendor/frames-protoinput");
    println!("cargo:rerun-if-changed=../../scripts/build_shared_input.py");
    println!("cargo:rerun-if-changed=../../native/windows/frames.manifest");
    // Native examples and unit-test executables need the same Windows capability
    // manifest as Frames for layered viewer children and common controls.
    #[cfg(windows)]
    winresource::WindowsResource::new()
        .set_manifest_file("../../native/windows/frames.manifest")
        .compile()
        .expect("compile native Windows capability manifest");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("shared-input");
    let status = std::process::Command::new("python")
        .args(["../../scripts/build_shared_input.py", "--output"])
        .arg(&out)
        .status()
        .expect("run native input build");
    assert!(status.success(), "native shared input adapter build failed");
    println!("cargo:rustc-cfg=frames_shared_input");
    println!(
        "cargo:rustc-env=FRAMES_INPUT_ARTIFACT={}",
        out.join("Release/FramesInput.dll").display()
    );
}
