//! Static SDL's Objective-C code uses Clang's @available runtime
//! (`__isPlatformVersionAtLeast`). Rust links with -nodefaultlibs, which
//! leaves it out, so link the selected Apple Clang runtime explicitly.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
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
}
