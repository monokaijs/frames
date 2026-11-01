use sha2::{Digest, Sha256};
use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=FRAMES_MEDIA_SDK");
    println!("cargo:rerun-if-changed=src/codec/native.c");
    let windows = env::var("CARGO_CFG_TARGET_OS").unwrap() == "windows";
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("../..");
    let sdk = env::var_os("FRAMES_MEDIA_SDK")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            root.join(".media-runtime")
                .join(if windows { "windows" } else { "macos" })
        });
    assert!(
        sdk.join("include/libavcodec/avcodec.h").is_file(),
        "Prepare the bundled media SDK first: python scripts/prepare_media.py (Windows) or scripts/prepare_media_macos.sh (macOS)"
    );
    cc::Build::new()
        .std("c11")
        .file("src/codec/native.c")
        .include(sdk.join("include"))
        .warnings(true)
        .compile("frames_media");
    if env::var("CARGO_CFG_TARGET_OS").unwrap() == "macos" {
        for framework in ["VideoToolbox", "CoreVideo", "CoreMedia", "CoreFoundation"] {
            println!("cargo:rustc-link-lib=framework={framework}");
        }
    }
    let mut generated = String::new();
    if windows {
        println!("cargo:rustc-link-lib=dxguid");
        let mut files: Vec<_> = fs::read_dir(sdk.join("bin"))
            .unwrap()
            .map(|f| f.unwrap().path())
            .filter(|p| {
                p.file_name().is_some_and(|n| {
                    ["avcodec-", "avutil-", "swscale-", "swresample-"]
                        .iter()
                        .any(|prefix| n.to_string_lossy().starts_with(prefix))
                })
            })
            .collect();
        files.sort();
        assert!(!files.is_empty(), "No bundled codec DLLs");
        generated.push_str("pub const LIBRARIES: &[(&str, &[u8], &str)] = &[\n");
        for path in files {
            println!("cargo:rerun-if-changed={}", path.display());
            let hash = format!("{:x}", Sha256::digest(fs::read(&path).unwrap()));
            generated.push_str(&format!(
                "({:?}, include_bytes!({:?}), {:?}),\n",
                path.file_name().unwrap().to_str().unwrap(),
                path.canonicalize().unwrap(),
                hash
            ));
        }
        generated.push_str("];\n");
    }
    fs::write(
        PathBuf::from(env::var("OUT_DIR").unwrap()).join("media_runtime.rs"),
        generated,
    )
    .unwrap();
    println!(
        "cargo:rustc-env=FRAMES_MEDIA_SDK={}",
        sdk.canonicalize().unwrap().display()
    );
}
