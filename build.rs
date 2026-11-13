//! Embed the Windows resources: the app icon that Explorer, the taskbar and
//! Alt-Tab read off the executable itself. Without this the exe carries no
//! icon and every shortcut to it shows the generic default.

fn main() {
    println!("cargo:rerun-if-changed=assets/bayan.ico");
    println!("cargo:rerun-if-changed=build.rs");

    // wgpu-hal imports opengl32.dll and may import dxgi.dll as raw-dylibs,
    // which the loader resolves before main. Delay-load them so
    // `dll_search::harden_dll_search` runs first and a copy beside bayan.exe
    // is not on the search path. The VC runtime is not delay-loaded.
    // `/DELAYLOAD` is the MSVC linker's flag; the shipped build is MSVC.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os == "windows" && target_env == "msvc" {
        for dll in [
            "opengl32.dll",
            "dxgi.dll",
            "d3d12.dll",
            "dcomp.dll",
            "d3dcompiler_47.dll",
        ] {
            println!("cargo:rustc-link-arg=/DELAYLOAD:{dll}");
        }
        println!("cargo:rustc-link-lib=delayimp");
    }

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
