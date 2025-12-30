
<p align="center">
  <img src=".github/readme/logotrans.png" alt="RiverDeck logo" />
</p>

---

<p align="center">
  <strong>Cross-platform software for your Elgato Stream Deck</strong>
</p>

<p align="center">
  <img src=".github/readme/mainmenu.png" alt="Main menu" /><br>
  <a href="#showcase">More screenshots</a>
</p>

RiverDeck is a native Rust desktop application for using stream controller devices like the Elgato Stream Deck on Linux, Windows, and macOS.

RiverDeck supports **RiverDeck-native** plugins (recommended), and also has **best-effort compatibility** with Stream Deck SDK plugin bundles and the [OpenAction](https://openaction.amankhanna.me/) ecosystem. Plugin compatibility depends on your OS and how the plugin is packaged (see [Plugins](#plugins)).

Only Elgato hardware is officially supported, but plugins are available for support for other hardware vendors.

RiverDeck is made by Ethan Wright (`sulrwin`) and is forked from OpenDeck. A lot of fundamental components and systems have been changed from OpenDeck — thanks to OpenDeck and its contributors for the original work.

Special thanks go to the developers of the [elgato-streamdeck](https://github.com/OpenActionAPI/rust-elgato-streamdeck) Rust library, [Wine](https://www.winehq.org/), and [Phosphor Icons](https://phosphoricons.com/).

### Why use RiverDeck?

- **Modern native UI**: RiverDeck brings a modern interface and implements many features users expect from the official software, plus RiverDeck-specific improvements.

- **Stream Deck plugins**: RiverDeck supports the majority of the Stream Deck plugins that users of the Elgato ecosystem are already familiar with, unlike other third-party software which is much more limited (e.g. streamdeck-ui, StreamController, Boatswain etc).
- **Cross-platform**: RiverDeck supports Linux alongside Windows and macOS. Profile files are portable between platforms with no changes needed.
- **Feature-packed**: From Multi Actions and Toggle Actions to switching profiles when you switch apps and brightness control, RiverDeck has all the features you'd expect from stream controller software.
- **Open source**: RiverDeck source code is licensed under the GNU General Public License, allowing anyone to view it and improve it for feature, stability, privacy or security reasons.
- **Written in Rust**: RiverDeck is built in Rust for performance and safety. (Some built-in plugin build scripts are written in TypeScript and run via Deno.)

## Showcase

<p align="center">
  <img src=".github/readme/mainmenu.png" alt="Main menu" /><br>
  <img src=".github/readme/mainmenu-plugins-profiles.png" alt="Plugins and profiles" />
</p>

## Installation

### Linux

> [!TIP]
> If you're using a Debian, Ubuntu, Fedora, Fedora Atomic, openSUSE or Arch-based distribution, you can try the new automated installation script:
> ```bash
> bash <(curl -sSL https://raw.githubusercontent.com/sulrwin/RiverDeck/main/install_riverdeck.sh)
> ```
> The script installs RiverDeck from a released `.deb`/`.rpm` file, the AUR, or Flathub (if available), and also installs + reloads the udev rules required for Stream Deck device access.

- Download the latest release from [GitHub Releases](https://github.com/sulrwin/RiverDeck/releases/latest).
- Install RiverDeck using your package manager of choice.
- Install the udev subsystem rules (required for device access) from [`packaging/linux/40-streamdeck.rules`](packaging/linux/40-streamdeck.rules):
	- If you're using a `.deb` or `.rpm` release artifact, this file should be installed automatically.
	- Otherwise, download and copy it to the correct location with `sudo cp 40-streamdeck.rules /etc/udev/rules.d/`.
	- In both cases, you will need to reload your udev subsystem rules with `sudo udevadm control --reload-rules && sudo udevadm trigger`.

> [!NOTE]
> **Plugins on Linux are native-first.** RiverDeck currently requires plugins to provide a native Linux executable (via `CodePathLin` / `CodePaths`) and does not run Windows-only plugins via Wine on Linux.

> [!NOTE]
> If Flatpak is your only option, check whether RiverDeck is available on Flathub. Please note that you still need to install the udev subsystem rules as described above.

### Windows

- Download the latest release (`.exe` or `.msi`) from [GitHub Releases](https://github.com/sulrwin/RiverDeck/releases/latest).
- Double-click the downloaded file to run the installer.

### macOS

- Download the latest release from [GitHub Releases](https://github.com/sulrwin/RiverDeck/releases/latest).
- If you downloaded a `.dmg`, open the downloaded disk image and drag the application inside into your Applications folder; otherwise, extract the `.tar.gz` to your Applications folder.
- Open the installed application. Note: if you receive a warning about RiverDeck being distributed by an unknown developer, *right-click the app in Finder and then click Open* to suppress the warning.
- If you intend to use plugins that are only compiled for Windows, you will need to have [Wine](https://www.winehq.org/) installed on your system.

## Plugins

RiverDeck supports multiple plugin “ecosystems”, but **not every plugin will work on every OS**.

- **RiverDeck-native plugins (recommended)**: native binaries designed for RiverDeck. A minimal starting point lives at [`plugins/io.github.sulrwin.riverdeck.template.sdPlugin/`](plugins/io.github.sulrwin.riverdeck.template.sdPlugin/).
- **Stream Deck SDK bundles (`.streamDeckPlugin`)**: RiverDeck can install these (including marketplace deep links) and supports many of them, but compatibility varies:
	- On **Linux**, plugins must include a native Linux binary.
	- On **Windows**, Windows plugins generally work best.
	- On **macOS**, some Windows-only plugins may work via Wine (if installed natively).
- **OpenAction Marketplace**: RiverDeck can handle OpenAction marketplace installs and will install the plugin if it can find a suitable downloadable bundle.

You can manage plugins in-app via **Settings → Plugins**.

RiverDeck also supports installing Stream Deck **icon packs** (`.streamDeckIconPack`).

## Support

### How do I...?

To edit an action's settings, left-click on it to display its *property inspector*. To remove an action, right-click on it and choose "Delete" from the context menu.

To edit an action's appearance, right-click on it and select "Edit" from the context menu. You can then customise the image and text for each of its states. Left-click on the image to choose an image from your filesystem or right-click on the image to reset it to the plugin-provided default.

To select another device, or to switch profiles, use the dropdowns in the top right corner. You can organise profiles into folders by prefixing the profile name with the folder name and a forward slash. You can also configure automatically switching to a profile when a specific application's window is active.

To change other options, open Settings. From here, you can also view information about your version of RiverDeck or open the configuration and log directories. To add or remove plugins, visit the Plugins tab.

### Troubleshooting

- Ensure you are running the latest version of RiverDeck, as well as recent versions of related software (e.g. Spotify or OBS, or your operating system and Wine).
- Check the pinned [GitHub Issues](https://github.com/sulrwin/RiverDeck/issues) to see if there's a fix for your problem already.
- Check the RiverDeck log file for any important messages. This file should be included with any support request.
	- You can also run RiverDeck from the terminal to see the logs directly if it's easier than finding the log file or if the log file is empty or missing details.
	- For issues with plugins, you can also check the plugin's logs (in the same folder, sometimes as well as a file named `plugin.log` or similar in the plugin's own folder).
	- The log directory can be opened from the settings page of RiverDeck, or alternatively located manually at paths similar to the below:
		- Linux: `~/.local/share/io.github.sulrwin.riverdeck/logs/`
		- Flatpak: `~/.var/app/io.github.sulrwin.riverdeck/data/io.github.sulrwin.riverdeck/logs/`
		- Windows: `%APPDATA%\io.github.sulrwin.riverdeck\logs\`
		- macOS: `~/Library/Application Support/io.github.sulrwin.riverdeck/logs/`
- When trying to run Windows-only plugins on macOS, please ensure you have a recent version of Wine installed (natively; the Wine Flatpak is not supported).
- If your device isn't showing up, ensure you have the correct permissions to access it (e.g. on Linux, installing udev subsystem rules and restarting your system), and that you have restarted RiverDeck since connecting it.

### Support forums

- [GitHub Issues](https://github.com/sulrwin/RiverDeck/issues)

### Building from source / contributing

> [!TIP]
> The development guide for agents present in [AGENTS.md](AGENTS.md) also serves as a useful introduction to the codebase for humans.

RiverDeck's UI is a native Rust/egui application. To build/run it you just need a Rust toolchain (and on Linux, `libudev` for Stream Deck access, plus GTK3/WebKitGTK for the `riverdeck-pi` webview helper).

- Run the app: `cargo run -p riverdeck-egui`
- Build binaries: `cargo build -p riverdeck-egui -p riverdeck-pi`

On Debian/Ubuntu, you’ll typically need WebKitGTK + GTK3 development packages:

```bash
sudo apt-get install -y libwebkit2gtk-4.1-dev libgtk-3-dev libudev-dev xdg-utils
```

On Linux, the default `riverdeck-egui` build enables tray support, which requires the system library **`libxdo`**:
- Arch: `sudo pacman -S --needed xdotool`
- Debian/Ubuntu: `sudo apt-get install -y libxdo-dev`
- Or disable tray support: `cargo build -p riverdeck-egui --no-default-features`

RiverDeck no longer depends on Tauri.

Before each commit, please ensure that all of the following are completed:
1. Rust code has been linted using `cargo clippy --workspace --all-targets -- -D warnings`
2. Rust code has been formatted using `cargo fmt --all`

When submitting contributions, please adhere to the [Conventional Commits specification](https://conventionalcommits.org/) for commit messages. You will also need to [sign your commits](https://docs.github.com/en/authentication/managing-commit-signature-verification/signing-commits). Feel free to reach out on the support channels above for guidance when contributing!

RiverDeck is licensed under the GNU General Public License version 3.0 or later. For more details, see the LICENSE.md file.

## RiverDeck-native plugins

RiverDeck is moving to a **RiverDeck-native** plugin ecosystem (Linux-first).

- Template: [`plugins/io.github.sulrwin.riverdeck.template.sdPlugin/README.md`](plugins/io.github.sulrwin.riverdeck.template.sdPlugin/README.md)
- Built-in plugin sources: [`plugins/`](plugins/)
