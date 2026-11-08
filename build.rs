fn main() {
    slint_build::compile_with_config(
        "ui/app.slint",
        slint_build::CompilerConfiguration::new().with_style("fluent-dark".into()),
    )
    .unwrap();
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        println!("cargo:rerun-if-changed=ui/afterbeat.ico");
        println!("cargo:rerun-if-changed=ui/taskbar-running.ico");
        println!("cargo:rerun-if-changed=ui/taskbar-stop.ico");
        println!("cargo:rerun-if-changed=ui/afterbeat.manifest");
        winresource::WindowsResource::new()
            .set("ProductName", "Afterbeat")
            .set("FileDescription", "Afterbeat")
            .set("OriginalFilename", "afterbeat.exe")
            .set_icon("ui/afterbeat.ico")
            .set_icon_with_id("ui/taskbar-running.ico", "2")
            .set_icon_with_id("ui/taskbar-stop.ico", "3")
            .set_manifest_file("ui/afterbeat.manifest")
            .compile()
            .unwrap();
    }
}
