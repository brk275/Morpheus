fn main() {
    slint_build::compile("ui/app-window.slint").expect("failed to compile Morpheus UI");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
        && std::env::var_os("CARGO_FEATURE_NATIVE_CALLS").is_some()
    {
        // Keep WebRTC's Objective-C categories in the final macOS executable.
        println!("cargo:rustc-link-arg-bins=-ObjC");
    }
}
