fn main() {
    slint_build::compile("../morpheus-app/ui/app-window.slint")
        .expect("failed to compile Morpheus UI");
}
