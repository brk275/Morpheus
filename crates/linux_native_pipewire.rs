// Linux source-build integration. No commands, downloads, source preparation,
// runtime loader overrides, or filesystem writes occur in this Cargo hook.
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, env, fs, io::Read, os::unix::fs::MetadataExt, path::Path};

const NATIVE_SONAME: &str = "libmorpheus-pipewire-0.3.so.0";
const APPIMAGE_SONAME: &str = "libpipewire-0.3.so.0";
const MAX_LIBRARY: u64 = 64 * 1024 * 1024;

fn regular(path: &Path, owner: u32, limit: u64) -> Vec<u8> {
    let expected = fs::symlink_metadata(path).expect("prepared PipeWire input missing");
    assert!(
        expected.is_file()
            && expected.uid() == owner
            && expected.mode() & 0o022 == 0
            && expected.len() <= limit,
        "prepared PipeWire file rejected"
    );
    let file = fs::File::open(path).expect("open prepared PipeWire input");
    let actual = file.metadata().expect("prepared PipeWire metadata");
    assert!(
        actual.dev() == expected.dev() && actual.ino() == expected.ino(),
        "prepared PipeWire file changed"
    );
    let mut data = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut data)
        .expect("read prepared PipeWire input");
    assert!(
        data.len() as u64 == actual.len() && data.len() as u64 <= limit,
        "prepared PipeWire size changed"
    );
    data
}

pub fn configure(kind: &str) {
    assert!(
        matches!(kind, "bins" | "tests"),
        "unsupported PipeWire link target kind"
    );
    println!("cargo:rerun-if-env-changed=MORPHEUS_PIPEWIRE_CLIENT_RECEIPT");
    println!("cargo:rerun-if-env-changed=MORPHEUS_PIPEWIRE_CLIENT_VARIANT");
    let variant = env::var("MORPHEUS_PIPEWIRE_CLIENT_VARIANT").unwrap_or_default();
    // The old, explicitly hosted A/B diagnostic must still link the stock
    // client before its separate exact-loader experiment. It is not the normal
    // source-build or ordinary capture acceptance path.
    if variant == "system-probe" {
        assert!(
            kind == "tests"
                && env::var("MORPHEUS_WAYLAND_STARTUP_PROBE").as_deref() == Ok("1")
                && env::var("GITHUB_ACTIONS").as_deref() == Ok("true")
                && env::var("RUNNER_ENVIRONMENT").as_deref() == Ok("github-hosted"),
            "stock PipeWire is restricted to the explicit hosted comparison"
        );
        return;
    }
    assert!(
        matches!(variant.as_str(), "native" | "appimage"),
        "prepare Linux capture with scripts/build_linux_native.py before Cargo"
    );
    assert!(
        env::var("HOST").as_deref() == Ok("x86_64-unknown-linux-gnu")
            && env::var("TARGET").as_deref() == Ok("x86_64-unknown-linux-gnu"),
        "prepared PipeWire requires the reviewed Ubuntu24 Linux x64 target"
    );
    let path = env::var_os("MORPHEUS_PIPEWIRE_CLIENT_RECEIPT")
        .map(std::path::PathBuf::from)
        .expect("prepared PipeWire receipt missing");
    assert!(
        path.is_absolute()
            && fs::canonicalize(&path).ok().as_ref() == Some(&path)
            && path.file_name().and_then(|v| v.to_str()) == Some("build-client.txt"),
        "prepared PipeWire receipt path rejected"
    );
    let directory = path.parent().expect("prepared PipeWire directory");
    let owner = fs::metadata(env::var_os("OUT_DIR").expect("Cargo output directory"))
        .expect("Cargo output metadata")
        .uid();
    let metadata = fs::symlink_metadata(directory).expect("prepared PipeWire directory metadata");
    assert!(
        owner != 0 && metadata.is_dir() && metadata.uid() == owner && metadata.mode() & 0o077 == 0,
        "prepared PipeWire directory rejected"
    );
    let text = String::from_utf8(regular(&path, owner, 4096)).expect("ASCII PipeWire receipt");
    assert!(
        text.is_ascii() && text.ends_with('\n'),
        "invalid PipeWire receipt encoding"
    );
    let mut fields = BTreeMap::new();
    for line in text.lines() {
        let (key, value) = line
            .split_once('=')
            .expect("invalid PipeWire receipt field");
        assert!(
            !key.is_empty() && !value.is_empty() && fields.insert(key, value).is_none(),
            "duplicate or empty PipeWire receipt field"
        );
    }
    let soname = if variant == "native" {
        NATIVE_SONAME
    } else {
        APPIMAGE_SONAME
    };
    let fixed = [
        ("schema", "1"),
        ("variant", variant.as_str()),
        ("soname", soname),
        ("source_version", "1.0.5-1ubuntu3.3"),
        (
            "upstream_commit",
            "800a25a01f99156d7d954f4d7fe1b0a6cf8ae7e3",
        ),
        (
            "upstream_patch_sha256",
            "d46d38c905bf18e82515ade96fb4a8ab3fbd62fd0784b92655b21266de343584",
        ),
        (
            "patched_source_sha256",
            "9b47ad0083b39ef83fcb8bae4beb0d9dfc462689862eac46127f67e631d62a45",
        ),
        (
            "original_archive_sha256",
            "d5e5f3d0b8460e5711c1571c500156fd61768ba55a062eaeb94356abdb955a56",
        ),
        (
            "debian_archive_sha256",
            "a1650f8e95859b3a8057ea95dad7e0509bfed9896db5b0d17c70910c4e9a3ff0",
        ),
        (
            "dsc_sha256",
            "8c9f84687253a01bd787431bf8ef04f4c9306db1a7c0dda6c9564f17da6a7c02",
        ),
        (
            "native_meson_sha256",
            if variant == "native" {
                "01e57c7f2b10a615cb07f64c2dbf3eac63383c1bcd242c36882aecfbd8c7b4ce"
            } else {
                "none"
            },
        ),
    ];
    assert_eq!(
        fields.len(),
        fixed.len() + 2,
        "unknown PipeWire receipt field"
    );
    for (key, expected) in fixed {
        assert!(
            fields.get(key).copied() == Some(expected),
            "PipeWire source identity changed"
        );
    }
    let hex = |value: &str| {
        value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    let digest = fields.get("sha256").expect("PipeWire digest missing");
    let build_id = fields.get("build_id").expect("PipeWire build-ID missing");
    assert!(
        digest.len() == 64
            && hex(digest)
            && (16..=128).contains(&build_id.len())
            && build_id.len() % 2 == 0
            && hex(build_id),
        "invalid PipeWire artifact identity"
    );
    let library = directory.join(soname);
    let data = regular(&library, owner, MAX_LIBRARY);
    assert!(
        format!("{:x}", Sha256::digest(&data)) == *digest,
        "prepared PipeWire digest changed"
    );
    println!("cargo:rerun-if-changed={}", path.display());
    println!("cargo:rerun-if-changed={}", library.display());
    if variant == "native" {
        let directory_text = directory.to_str().expect("UTF-8 PipeWire prefix");
        assert!(
            directory_text.len() <= 4096
                && !directory_text
                    .chars()
                    .any(|c| c.is_control() || matches!(c, ':' | ',' | '$' | '\\')),
            "PipeWire prefix cannot be represented as a normal ELF RUNPATH"
        );
        let alias = directory.join("libpipewire-0.3.so");
        assert!(
            fs::canonicalize(&alias).ok() == Some(library.clone()),
            "native PipeWire link alias changed"
        );
        println!("cargo:rustc-link-search=native={directory_text}");
        // Generic link-arg also covers native lib unit-test harnesses, not only
        // named integration-test targets. The app has its own hook because
        // dependency build-script link arguments do not reach its final link.
        println!("cargo:rustc-link-arg=-Wl,--enable-new-dtags,-rpath,{directory_text}");
        println!(
            "cargo:rustc-env=MORPHEUS_NATIVE_PIPEWIRE_ARTIFACT={}",
            directory.join("artifact.json").display()
        );
    }
}
