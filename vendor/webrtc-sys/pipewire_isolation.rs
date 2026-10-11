// Morpheus modification, licensed under Apache-2.0 like webrtc-sys.
// See MORPHEUS_PATCH.md. This operates only on a verified OUT_DIR copy.
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

const ARCHIVE_SHA256: &str = "9e377235c75d5f8992202187216187644b00a1bc71aa05f93841f8559b09281a";
const PREFIX: &str = "morpheus_webrtc_";
const INVENTORY: &str = include_str!("pipewire-symbols.txt");

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Symbol {
    member: String,
    name: String,
    kind: String,
    // Preserve symbol type, address and size, including unrelated symbols.
    details: Vec<String>,
}

pub fn validate_configuration(arch: &str) {
    assert!(
        arch == "x86_64"
            && env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu")
            && env::var("HOST").as_deref() == Ok("x86_64-unknown-linux-gnu")
            && !webrtc_sys_build::use_debug()
            && webrtc_sys_build::custom_dir().is_none()
            && webrtc_sys_build::WEBRTC_TAG == "webrtc-51ef663",
        "Morpheus PipeWire isolation requires the reviewed Linux x64 release WebRTC archive"
    );
    // Cargo dev/test profiles also select the release WebRTC archive upstream.
    assert_eq!(webrtc_sys_build::webrtc_triple(), "linux-x64-release");
}

fn hash(path: &Path) -> String {
    let mut file = fs::File::open(path).expect("open WebRTC archive for verification");
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let count = file
            .read(&mut buffer)
            .expect("read WebRTC archive for verification");
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    format!("{:x}", digest.finalize())
}

fn parse_symbols(output: &str) -> Vec<Symbol> {
    let mut member = None;
    let mut symbols = Vec::new();
    for line in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if let Some(header) = line.strip_suffix(':') {
            // GNU nm: archive.a[member.o]:; LLVM nm: member.o:.
            let name = match header.rsplit_once('[') {
                Some((_, tail)) => tail
                    .strip_suffix(']')
                    .expect("invalid archive member header"),
                None => header,
            };
            assert!(!name.is_empty(), "empty archive member header");
            member = Some(name.to_owned());
            continue;
        }
        let fields: Vec<_> = line.split_whitespace().collect();
        assert!(
            (2..=4).contains(&fields.len()),
            "unexpected nm symbol record"
        );
        assert_eq!(fields[1].len(), 1, "unexpected nm symbol type");
        symbols.push(Symbol {
            member: member.clone().expect("symbol without archive member"),
            name: fields[0].to_owned(),
            kind: fields[1].to_owned(),
            details: fields[2..]
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
        });
    }
    assert!(!symbols.is_empty(), "empty WebRTC symbol inventory");
    symbols.sort();
    symbols
}

fn symbols(path: &Path) -> Vec<Symbol> {
    let result = Command::new("/usr/bin/nm")
        .args(["--format=posix", "--extern-only", "--no-demangle"])
        .arg(path)
        .env("LC_ALL", "C")
        .output()
        .expect("GNU nm is required for Linux PipeWire isolation");
    assert!(result.status.success(), "WebRTC symbol inspection failed");
    parse_symbols(std::str::from_utf8(&result.stdout).expect("invalid nm output"))
}

fn no_address_significance_table(path: &Path) -> bool {
    let result = Command::new("/usr/bin/readelf")
        .args(["--section-headers", "--wide"])
        .arg(path)
        .env("LC_ALL", "C")
        .output()
        .expect("GNU readelf is required for Linux PipeWire isolation");
    assert!(result.status.success(), "WebRTC section inspection failed");
    let output = std::str::from_utf8(&result.stdout).expect("invalid readelf output");
    assert!(!output.is_empty(), "empty WebRTC section inventory");
    !output.contains(".llvm_addrsig")
}

fn validate_inventory(symbols: &[Symbol]) -> Vec<String> {
    let mut actual: Vec<_> = symbols
        .iter()
        .filter(|symbol| symbol.name.starts_with("pw_"))
        .map(|symbol| format!("{} {} {}", symbol.member, symbol.name, symbol.kind))
        .collect();
    let mut expected: Vec<_> = INVENTORY.lines().map(str::to_owned).collect();
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected, "pinned WebRTC PipeWire inventory changed");
    assert!(
        symbols
            .iter()
            .all(|symbol| !symbol.name.starts_with(PREFIX)),
        "WebRTC already contains isolation symbols"
    );
    let mut names: Vec<_> = symbols
        .iter()
        .filter(|symbol| symbol.name.starts_with("pw_") && symbol.kind == "W")
        .map(|symbol| symbol.name.clone())
        .collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), 33, "unexpected PipeWire stub inventory");
    names
}

fn renamed(mut symbols: Vec<Symbol>) -> Vec<Symbol> {
    for symbol in &mut symbols {
        if symbol.name.starts_with("pw_") {
            symbol.name.insert_str(0, PREFIX);
        }
    }
    symbols.sort();
    symbols
}

pub fn isolate(source: &Path) -> PathBuf {
    println!("cargo:rerun-if-changed=pipewire_isolation.rs");
    println!("cargo:rerun-if-changed=pipewire-symbols.txt");
    println!("cargo:rerun-if-changed={}", source.display());
    let directory = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory"))
        .join("morpheus-pipewire");
    fs::create_dir_all(&directory).expect("create isolated WebRTC directory");
    let snapshot = directory.join("upstream.a");
    fs::copy(source, &snapshot).expect("copy WebRTC archive without modifying its cache");
    assert_eq!(hash(&snapshot), ARCHIVE_SHA256, "unreviewed WebRTC archive");
    let before = symbols(&snapshot);
    let names = validate_inventory(&before);
    let mapping = names
        .iter()
        .map(|name| format!("{name} {PREFIX}{name}\n"))
        .collect::<String>();
    let map_path = directory.join("pipewire-redefine.txt");
    fs::write(&map_path, mapping).expect("write PipeWire symbol mapping");
    let candidate = directory.join("isolated.a");
    let result = Command::new("/usr/bin/objcopy")
        .arg("--enable-deterministic-archives")
        // This optional LLVM table contains raw symbol indexes. GNU objcopy
        // may reorder those indexes; absence conservatively marks every
        // symbol address-significant instead of retaining stale metadata.
        .arg("--remove-section=.llvm_addrsig")
        .arg(format!("--redefine-syms={}", map_path.display()))
        // Whole-archive rewrite includes every member's undefined references.
        // Both paths are explicit: the downloaded cache is never an output.
        .arg(&snapshot)
        .arg(&candidate)
        .env("LC_ALL", "C")
        .output()
        .expect("GNU objcopy is required for Linux PipeWire isolation");
    assert!(result.status.success(), "WebRTC PipeWire isolation failed");
    assert!(
        no_address_significance_table(&candidate),
        "WebRTC retained index-based address-significance metadata"
    );
    // This compares all global symbols, not just the 33 weak definitions:
    // all 46 references must change, all other symbols must stay the same.
    assert!(
        symbols(&candidate) == renamed(before),
        "WebRTC symbol isolation was incomplete"
    );
    assert_eq!(
        hash(&snapshot),
        ARCHIVE_SHA256,
        "WebRTC snapshot was modified"
    );
    fs::rename(&candidate, directory.join("libwebrtc.a")).expect("publish isolated WebRTC archive");
    fs::remove_file(snapshot).expect("remove temporary WebRTC snapshot");
    directory
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gnu_and_llvm_headers_and_preserves_all_member_references() {
        let gnu = "input.a[stub.o]:\npw_init W 0 c\ninput.a[user.o]:\npw_init U\nother T 2 4\n";
        let llvm = "stub.o:\npw_init W 0 c\nuser.o:\npw_init U\nother T 2 4\n";
        let original = parse_symbols(gnu);
        assert_eq!(original, parse_symbols(llvm));
        let result = renamed(original.clone());
        assert_eq!(
            result
                .iter()
                .filter(|symbol| symbol.name == "morpheus_webrtc_pw_init")
                .count(),
            2
        );
        let unrelated_unchanged = result
            .iter()
            .any(|symbol| symbol.name == "other" && symbol.details == ["2", "4"]);
        assert!(unrelated_unchanged);
        let mut definitions_only = original;
        definitions_only
            .iter_mut()
            .find(|symbol| symbol.kind == "W")
            .unwrap()
            .name = "morpheus_webrtc_pw_init".into();
        definitions_only.sort();
        assert_ne!(
            result, definitions_only,
            "a definition-only rewrite must be rejected"
        );
    }

    #[test]
    fn fixed_inventory_requires_all_33_definitions_and_46_references() {
        let symbols: Vec<_> = INVENTORY
            .lines()
            .map(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                Symbol {
                    member: fields[0].into(),
                    name: fields[1].into(),
                    kind: fields[2].into(),
                    details: vec![],
                }
            })
            .collect();
        assert_eq!(validate_inventory(&symbols).len(), 33);
        assert_eq!(
            symbols.iter().filter(|symbol| symbol.kind == "U").count(),
            46
        );
        let mut incomplete = symbols.clone();
        incomplete.remove(0);
        assert!(std::panic::catch_unwind(|| validate_inventory(&incomplete)).is_err());
        let mut unknown = symbols;
        unknown[0].name = "pw_future_unreviewed".into();
        assert!(std::panic::catch_unwind(|| validate_inventory(&unknown)).is_err());
    }
}
