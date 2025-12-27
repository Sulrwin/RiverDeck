fn main() {
    // `tray-icon` on Linux pulls in `libxdo` (linked as `-lxdo`) via its X11 helpers.
    // Without it, the build fails late at link-time with a confusing error.
    //
    // We detect that case early and provide actionable guidance.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let tray_enabled = std::env::var_os("CARGO_FEATURE_TRAY").is_some();

    if target_os == "linux" && tray_enabled {
        let candidates = [
            "/usr/lib/libxdo.so",
            "/usr/lib64/libxdo.so",
            "/usr/local/lib/libxdo.so",
            "/usr/local/lib64/libxdo.so",
            "/lib/libxdo.so",
            "/lib64/libxdo.so",
        ];

        let found = candidates.iter().any(|p| std::path::Path::new(p).exists());
        if !found {
            panic!(
                "\nMissing system library `libxdo` required for the `tray` feature on Linux.\n\n\
Install it via your distro packages:\n\
  - Arch: `sudo pacman -S --needed xdotool`\n\
  - Debian/Ubuntu: `sudo apt-get install -y libxdo-dev`\n\
  - Fedora: `sudo dnf install -y xdotool xdotool-devel` (package names may vary)\n\n\
Or build without tray support:\n\
  `cargo build -p riverdeck-egui --no-default-features`\n"
            );
        }
    }
}
