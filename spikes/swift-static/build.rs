//! Builds the SwiftPM static library and links it with the minimal Swift
//! runtime flags. The Swift runtime ships in macOS (14+), so only the SDK's
//! `.tbd` stubs are needed at link time and `/usr/lib/swift` at run time.
use std::{path::PathBuf, process::Command};

fn output(command: &mut Command) -> String {
    let result = command.output().expect("failed to run command");
    assert!(result.status.success(), "{command:?} failed: {}", String::from_utf8_lossy(&result.stderr));
    String::from_utf8(result.stdout).unwrap().trim().to_owned()
}

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let package = root.join("FramesMac");
    println!("cargo:rerun-if-changed={}", package.join("Package.swift").display());
    println!("cargo:rerun-if-changed={}", package.join("Sources").display());
    let profile = if std::env::var("PROFILE").as_deref() == Ok("release") { "release" } else { "debug" };
    let status = Command::new("swift")
        .args(["build", "-c", profile, "--package-path"])
        .arg(&package)
        .status()
        .expect("swift build");
    assert!(status.success(), "swift build failed");

    println!("cargo:rustc-link-search=native={}", package.join(".build").join(profile).display());
    println!("cargo:rustc-link-lib=static=FramesMac");
    // Autolink entries in the Swift objects name -lswiftCore, -lswift_Concurrency,
    // frameworks, etc.; the linker resolves them against the SDK's .tbd stubs.
    let sdk = output(Command::new("xcrun").args(["--sdk", "macosx", "--show-sdk-path"]));
    println!("cargo:rustc-link-search=native={sdk}/usr/lib/swift");
    // The OS copy of the Swift runtime is found at load time through this rpath.
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
}
