//! Bakes the app icon into the .exe as a Windows resource.
//!
//! This is what Explorer, Alt-Tab, installer shortcuts and — the reason it
//! matters most — a *pinned* taskbar button draw. Those read the executable's
//! embedded icon resource, which is a different thing from the window icon
//! `main.rs` sets at runtime: the runtime one only applies while the app is
//! actually running, so without this the pinned shortcut falls back to a blank
//! placeholder.
//!
//! `assets/app.ico` is generated from `assets/icon.png` by `assets/make_ico.py`
//! and carries every size Windows picks between (16 through 256). Re-run that
//! script whenever the source png changes.

fn main() {
    // Only Windows has an icon resource to embed; on other platforms this
    // build script deliberately does nothing.
    #[cfg(target_os = "windows")]
    {
        // Rerun when the icon changes — otherwise a regenerated .ico wouldn't
        // make it into the exe until something else forced a rebuild.
        println!("cargo:rerun-if-changed=assets/app.ico");

        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/app.ico");

        // Embedding needs a resource compiler (rc.exe from the Windows SDK, or
        // windres under mingw). If it isn't available, warn and carry on rather
        // than failing the build: the app is perfectly usable with a default
        // exe icon, and a hard error here would block compiling entirely.
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed the app icon: {e}");
        }
    }
}
