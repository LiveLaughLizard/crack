//! Embeds the application icon in Windows builds (the icon Explorer and the taskbar show).

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/icon.ico");
        if let Err(error) = resource.compile() {
            // A missing resource compiler shouldn't break the build; the exe just has no icon.
            println!("cargo:warning=couldn't embed the icon: {error}");
        }
    }
}
