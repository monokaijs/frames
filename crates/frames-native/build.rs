fn main() {
    println!("cargo:rustc-check-cfg=cfg(frames_shared_input)");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        // Static SDL Objective-C code uses Clang's @available runtime. Rust's
        // -nodefaultlibs link omits it, which surfaces when LTO retains Cocoa.
        println!("cargo:rerun-if-env-changed=DEVELOPER_DIR");
        let output = std::process::Command::new("xcrun")
            .args(["clang", "-print-resource-dir"])
            .output()
            .expect("locate the selected Apple Clang runtime");
        assert!(
            output.status.success(),
            "query Apple Clang resource directory"
        );
        let directory = std::path::PathBuf::from(
            String::from_utf8(output.stdout)
                .expect("Clang resource path is UTF-8")
                .trim(),
        )
        .join("lib/darwin");
        assert!(
            directory.join("libclang_rt.osx.a").is_file(),
            "missing macOS Clang runtime"
        );
        println!("cargo:rustc-link-search=native={}", directory.display());
        println!("cargo:rustc-link-lib=static=clang_rt.osx");
        return;
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc")
        || std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("x86_64")
    {
        return;
    }
    println!("cargo:rerun-if-changed=../../native/windows/frames.manifest");
    // Native examples and unit-test executables need the same Windows capability
    // manifest as Frames for layered viewer children and common controls.
    #[cfg(windows)]
    winresource::WindowsResource::new()
        .set_manifest_file("../../native/windows/frames.manifest")
        .compile()
        .expect("compile native Windows capability manifest");
    // The isolated-input adapter is built and used by frames-platform-windows.
}
