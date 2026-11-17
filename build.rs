// Embeds assets/icon.ico into the Windows exe (the icon shown in Explorer). The window and taskbar icon
// are set at runtime from assets/icon-256.png. A missing resource compiler only skips the exe icon.
fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");
    #[cfg(windows)]
    {
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
            let mut res = winresource::WindowsResource::new();
            res.set_icon("assets/icon.ico");
            res.set("ProductName", "M0on's patcher");
            res.set("FileDescription", "M0on's patcher");
            if let Err(err) = res.compile() {
                println!("cargo:warning=could not embed the exe icon: {err}");
            }
        }
    }
}
