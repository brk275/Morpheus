fn main() {
    // WebRTC's macOS static library contains Objective-C categories used at
    // runtime. Cargo does not forward LiveKit's own link argument to the final
    // integration-test executable, so retain those categories here.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
        && std::env::var_os("CARGO_FEATURE_NATIVE_RTC").is_some()
    {
        println!("cargo:rustc-link-arg-tests=-ObjC");
    }
}
