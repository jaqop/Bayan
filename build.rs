//! Embed the Windows resources: the app icon that Explorer, the taskbar and
//! Alt-Tab read off the executable itself. Without this the exe carries no
//! icon and every shortcut to it shows the generic default.

fn main() {
    println!("cargo:rerun-if-changed=assets/bayan.ico");
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/bayan.ico");
        // Shown in the file's Properties → Details pane.
        res.set("ProductName", "Bayan");
        res.set("FileDescription", "Bayan (بيان) — an Arabic-first terminal");
        res.set("LegalCopyright", "Copyright (c) 2026 jaqop — MIT");
        // A missing resource compiler must not break a source build: the icon
        // is cosmetic, the terminal is not.
        if let Err(e) = res.compile() {
            println!("cargo:warning=icon not embedded ({e})");
        }
    }
}
