//! On macOS the Swift runtime that frames-platform-macos links lives in
//! /usr/lib/swift. A dependency's `rustc-link-arg` does not reach this
//! binary, so the run-time search path is added here.
//!
//! On Windows this crate's own executables (`frames-next`, unit tests) get
//! Frames' capability manifest: layered child windows (the debug overlay)
//! and common controls need it, as in the app.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc")
    {
        return;
    }
    println!("cargo:rerun-if-changed=../../native/windows/frames.manifest");
    #[cfg(windows)]
    winresource::WindowsResource::new()
        .set_manifest_file("../../native/windows/frames.manifest")
        .compile()
        .expect("compile the Windows capability manifest");
}
