fn main() {
    println!("cargo:rerun-if-changed=ui/main.slint");
    println!("cargo:rerun-if-changed=ui/assets/tray.png");
    let config = slint_build::CompilerConfiguration::new()
        .with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/main.slint", config)
        .unwrap_or_else(|error| panic!("failed to compile Slint interface: {error}"));
}
