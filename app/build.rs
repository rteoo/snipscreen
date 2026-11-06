//! Embeds the app icon in snipscreen.exe, so Explorer, shortcuts and pinned
//! taskbar entries show it. The running window gets the same icon from
//! `app_icon()` in main.rs.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/snipscreen.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/snipscreen.ico");
    if let Err(e) = res.compile() {
        panic!("couldn't embed the icon in snipscreen.exe (is the Windows SDK's rc.exe installed?): {e}");
    }
}
