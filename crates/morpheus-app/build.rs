#[cfg(target_os = "linux")]
#[path = "../linux_native_pipewire.rs"]
mod linux_native_pipewire;

fn main() {
    println!("cargo:rerun-if-env-changed=MORPHEUS_PACKAGE_FORMAT");
    #[cfg(target_os = "linux")]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux")
        && std::env::var_os("CARGO_FEATURE_NATIVE_CALLS").is_some()
    {
        linux_native_pipewire::configure("bins");
    }
    let timeline_acceptance = timeline_acceptance_build();
    let macos_update_acceptance = macos_update_acceptance_build();
    let windows_update_acceptance = windows_update_acceptance_build();
    assert!(
        !timeline_acceptance
            || (macos_update_acceptance.is_none() && windows_update_acceptance.is_none()),
        "Acceptance variants cannot be combined"
    );
    assert!(
        macos_update_acceptance.is_none() || windows_update_acceptance.is_none(),
        "Acceptance variants cannot be combined"
    );
    let persistence = persistence_fingerprint(
        timeline_acceptance,
        macos_update_acceptance.as_deref(),
        windows_update_acceptance.as_deref(),
    );
    write_update_identity(&persistence);
    slint_build::compile("ui/app-window.slint").expect("failed to compile Morpheus UI");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
        && std::env::var_os("CARGO_FEATURE_NATIVE_CALLS").is_some()
    {
        // Keep WebRTC's Objective-C categories in the final macOS executable.
        println!("cargo:rustc-link-arg-bins=-ObjC");
    }
}

// Only an explicitly instrumented release build contains the synthetic Mac
// transaction entry point. A shipping binary cannot enable it by runtime
// environment or command line. Its run nonce also separates its cohort from
// shipping and from every other acceptance build.
fn macos_update_acceptance_build() -> Option<String> {
    println!("cargo::rustc-check-cfg=cfg(morpheus_macos_update_acceptance)");
    println!("cargo:rerun-if-env-changed=MORPHEUS_MAC_UPDATE_ACCEPTANCE_BUILD");
    let nonce = match std::env::var("MORPHEUS_MAC_UPDATE_ACCEPTANCE_BUILD") {
        Err(std::env::VarError::NotPresent) => return None,
        Ok(nonce) => nonce,
        Err(_) => panic!("Invalid Mac update acceptance build nonce"),
    };
    assert!(nonce.len() == 32 && nonce.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(std::env::var("PROFILE").as_deref(), Ok("release"));
    assert_eq!(std::env::var("CARGO_CFG_TARGET_OS").as_deref(), Ok("macos"));
    assert!(std::env::var_os("CARGO_FEATURE_NATIVE_CALLS").is_some());
    assert!(std::env::var_os("MORPHEUS_WINDOWS_UPDATE_ACCEPTANCE_BUILD").is_none());
    println!("cargo::rustc-cfg=morpheus_macos_update_acceptance");
    println!("cargo:rustc-env=MORPHEUS_MAC_UPDATE_ACCEPTANCE_NONCE={nonce}");
    Some(nonce)
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

// A private release-only nonce; ordinary builds neither compile the actor nor
// change their existing persistence fingerprint. Runtime env cannot enable it.
fn windows_update_acceptance_build() -> Option<String> {
    println!("cargo::rustc-check-cfg=cfg(morpheus_windows_update_acceptance)");
    println!("cargo:rerun-if-env-changed=MORPHEUS_WINDOWS_UPDATE_ACCEPTANCE_BUILD");
    match std::env::var("MORPHEUS_WINDOWS_UPDATE_ACCEPTANCE_BUILD") {
        Err(std::env::VarError::NotPresent) => None,
        Ok(nonce) => {
            assert!(nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit()));
            assert_eq!(std::env::var("PROFILE").as_deref(), Ok("release"));
            assert_eq!(
                std::env::var("CARGO_CFG_TARGET_OS").as_deref(),
                Ok("windows")
            );
            assert_eq!(
                std::env::var("CARGO_CFG_TARGET_ARCH").as_deref(),
                Ok("x86_64")
            );
            assert!(std::env::var_os("CARGO_FEATURE_NATIVE_CALLS").is_some());
            assert!(std::env::var_os("MORPHEUS_MAC_UPDATE_ACCEPTANCE_BUILD").is_none());
            println!("cargo::rustc-cfg=morpheus_windows_update_acceptance");
            println!("cargo:rustc-env=MORPHEUS_WINDOWS_UPDATE_ACCEPTANCE_NONCE={nonce}");
            Some(nonce)
        }
        Err(_) => panic!("Invalid private Windows update acceptance nonce"),
    }
}

// Conservative rollback cohort: source and locked dependencies that can write
// account/config data must match. App version alone is intentionally excluded.
// Changing these inputs requires a separately validated migration release.
fn persistence_fingerprint(
    timeline_acceptance: bool,
    macos_update_acceptance: Option<&str>,
    windows_update_acceptance: Option<&str>,
) -> String {
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
    if let Some(nonce) = macos_update_acceptance {
        hash.update(b"morpheus-macos-update-acceptance-instrumented-v1\n");
        hash.update(nonce.as_bytes());
        hash.update(b"\n");
    }
    if let Some(nonce) = windows_update_acceptance {
        hash.update(b"morpheus-windows-update-acceptance-v1\0");
        hash.update(nonce.as_bytes());
        hash.update([0]);
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
    let persistence = format!("{:x}", hash.finalize());
    println!("cargo:rustc-env=MORPHEUS_PERSISTENCE_COHORT={persistence}");
    persistence
}

// One immutable v2 record for native executable sections on Mac and Windows.
// The closed protocol separates helpers with ordinary-shutdown handoff guards
// from legacy helpers. It does not change the persistence cohort or journals.
// No runtime file or user setting can select a persistence cohort. Keep the
// existing cohort calculation above byte-for-byte equivalent.
fn write_update_identity(persistence: &str) {
    use std::{fs, path::PathBuf};
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    assert!(!version.is_empty() && version.len() <= 128);
    assert!(
        version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".+-".contains(&b))
    );
    assert!(matches!(
        arch.as_str(),
        "aarch64" | "x86_64" | "x86" | "arm"
    ));
    assert!(persistence.len() == 64 && persistence.bytes().all(|b| b.is_ascii_hexdigit()));
    // All interpolated scalars passed the closed ASCII checks above, so they
    // cannot inject JSON quoting, escapes or additional fields.
    let identity = format!(
        "{{\"protocol\":2,\"product\":\"io.github.brk275.morpheus\",\"version\":\"{version}\",\"arch\":\"{arch}\",\"persistence\":\"{persistence}\"}}"
    );
    let output =
        PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("morpheus-update-identity.json");
    fs::write(output, identity).unwrap();
}
