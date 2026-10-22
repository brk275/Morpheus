#[cfg(target_os = "linux")]
#[path = "../linux_native_pipewire.rs"]
mod linux_native_pipewire;

fn main() {
    #[cfg(target_os = "linux")]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux")
        && std::env::var_os("CARGO_FEATURE_NATIVE_CALLS").is_some()
    {
        linux_native_pipewire::configure("bins");
    }
    let timeline_acceptance = timeline_acceptance_build();
    persistence_fingerprint(timeline_acceptance);
    slint_build::compile("ui/app-window.slint").expect("failed to compile Morpheus UI");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
        && std::env::var_os("CARGO_FEATURE_NATIVE_CALLS").is_some()
    {
        // Keep WebRTC's Objective-C categories in the final macOS executable.
        println!("cargo:rustc-link-arg-bins=-ObjC");
    }
}

/// The instrumented real-room timeline acceptance build (see
/// tests/TIMELINE_10K.md and src/timeline_acceptance.rs). Only an explicit
/// `MORPHEUS_TIMELINE_ACCEPTANCE_BUILD=1` with the release profile sets
/// `cfg(morpheus_timeline_acceptance)`; every other build compiles none of
/// the acceptance driver. The variant is folded into the persistence
/// fingerprint below, so it never shares a cohort with a shipping build.
fn timeline_acceptance_build() -> bool {
    println!("cargo::rustc-check-cfg=cfg(morpheus_timeline_acceptance)");
    println!("cargo:rerun-if-env-changed=MORPHEUS_TIMELINE_ACCEPTANCE_BUILD");
    match std::env::var("MORPHEUS_TIMELINE_ACCEPTANCE_BUILD") {
        Err(std::env::VarError::NotPresent) => false,
        Ok(value) if value == "1" => {
            // The measurements must come from the optimized profile the app
            // ships with, never from a debug build.
            assert_eq!(
                std::env::var("PROFILE").as_deref(),
                Ok("release"),
                "MORPHEUS_TIMELINE_ACCEPTANCE_BUILD requires the release profile"
            );
            println!("cargo::rustc-cfg=morpheus_timeline_acceptance");
            true
        }
        _ => panic!("MORPHEUS_TIMELINE_ACCEPTANCE_BUILD accepts only 1"),
    }
}

// Conservative rollback cohort: source and locked dependencies that can write
// account/config data must match. App version alone is intentionally excluded.
// Changing these inputs requires a separately validated migration release.
fn persistence_fingerprint(timeline_acceptance: bool) {
    use sha2::{Digest, Sha256};
    use std::{fs, path::Path};
    fn hash_tree(path: &Path, root: &Path, hash: &mut Sha256) {
        // New or removed source files must invalidate the cohort too.
        println!("cargo:rerun-if-changed={}", path.display());
        let mut entries: Vec<_> = fs::read_dir(path)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for entry in entries {
            if entry.is_dir() {
                hash_tree(&entry, root, hash);
            } else {
                println!("cargo:rerun-if-changed={}", entry.display());
                hash.update(
                    entry
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .as_bytes(),
                );
                hash.update([0]);
                hash.update(fs::read(&entry).unwrap());
                hash.update([0]);
            }
        }
    }
    let root = Path::new("../..");
    let mut hash = Sha256::new();
    hash.update(b"morpheus-persistence-cohort-v1\0");
    for dir in ["crates/morpheus-matrix/src", "crates/morpheus-config/src"] {
        hash_tree(&root.join(dir), root, &mut hash);
    }
    // A path patch or a newly enabled already-resolved SDK feature need not
    // change Cargo.lock. Include those inputs rather than treating the lock
    // text alone as proof that persistence code is identical.
    hash_tree(&root.join("vendor"), root, &mut hash);
    for manifest in [
        "Cargo.toml",
        "crates/morpheus-app/Cargo.toml",
        "crates/morpheus-config/Cargo.toml",
        "crates/morpheus-matrix/Cargo.toml",
        "crates/morpheus-rtc/Cargo.toml",
    ] {
        let path = root.join(manifest);
        println!("cargo:rerun-if-changed={}", path.display());
        let source = fs::read_to_string(path).unwrap();
        let mut package_section = false;
        for line in source.lines() {
            if line.starts_with('[') {
                package_section = line == "[package]";
            }
            if !(manifest == "crates/morpheus-app/Cargo.toml"
                && package_section
                && line.starts_with("version = "))
            {
                hash.update(line.as_bytes());
                hash.update(b"\n");
            }
        }
    }
    let mut features: Vec<_> = std::env::vars()
        .filter(|(key, _)| key.starts_with("CARGO_FEATURE_"))
        .collect();
    features.sort();
    for (key, value) in features {
        hash.update(key.as_bytes());
        hash.update(b"=");
        hash.update(value.as_bytes());
        hash.update(b"\n");
    }
    // Ordinary builds hash exactly as before; an instrumented acceptance build
    // always gets a cohort of its own.
    if timeline_acceptance {
        hash.update(b"morpheus-timeline-acceptance-instrumented\n");
    }
    for file in ["src/drafts.rs", "src/voice_spaces.rs"] {
        println!("cargo:rerun-if-changed={file}");
        hash.update(fs::read(file).unwrap());
    }
    println!("cargo:rerun-if-changed=../../Cargo.lock");
    let lock = fs::read_to_string(root.join("Cargo.lock")).unwrap();
    for block in lock.split("[[package]]") {
        let app = block.lines().any(|line| line == "name = \"morpheus-app\"");
        for line in block.lines() {
            if !(app && line.starts_with("version = ")) {
                hash.update(line.as_bytes());
                hash.update(b"\n");
            }
        }
    }
    println!(
        "cargo:rustc-env=MORPHEUS_PERSISTENCE_COHORT={:x}",
        hash.finalize()
    );
}
