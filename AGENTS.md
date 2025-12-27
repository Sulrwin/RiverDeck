# RiverDeck Development Guide

## Architecture Overview

RiverDeck is a native Rust desktop application for controlling Elgato Stream Deck devices. RiverDeck is forked from OpenDeck. It's built with:
- **Backend core**: Rust (`crates/riverdeck-core`) - device communication, plugin management, WebSocket/HTTP servers
- **Main UI**: Rust egui (`crates/riverdeck-egui`) - native UI
- **Property Inspectors**: Rust `wry` helper (`crates/riverdeck-pi`) - embeds plugin HTML inspectors
- **Build Tool**: Cargo (Deno is only used by some plugin build scripts)

### Core Architecture Pattern

RiverDeck acts as a **host application** that communicates with **plugins** (separate processes):
1. Plugins connect via WebSocket (port dynamically allocated, starting from 57116)
2. Static assets served via `tiny_http` webserver (port = WebSocket port + 2)
3. Plugin property inspectors (HTML/JS) run in iframes and use separate WebSocket connections
4. Device button presses/releases trigger events sent to plugins via WebSocket

Key data flow: `Device (elgato-streamdeck crate) → Rust event handlers → WebSocket → Plugin process`

## Project Structure

```
crates/riverdeck-core/      # Rust backend core (no GUI framework dependency)
crates/riverdeck-egui/      # egui main UI
crates/riverdeck-pi/        # wry-based property inspector helper
packaging/                  # icons, desktop/metainfo, udev rules, flatpak manifests
plugins/                    # built-in plugin sources
```

## Critical Workflows

### Development Commands

```bash
# Run the native egui app
cargo run -p riverdeck-egui
```

### Pre-commit Requirements

Before commits, **always** run:
1. `cargo clippy --workspace --all-targets -- -D warnings`
2. `cargo fmt --all`

These are project standards, not suggestions.

### Built-in Plugins

Built-in plugins included in RiverDeck are Rust binaries. The `build.ts` script in each plugin compiles for multiple targets (x86_64/aarch64) and organizes binaries by OS.

## Key Conventions

### Type Synchronization

Core Rust types live in `crates/riverdeck-core/src/shared.rs`:
- `Context`, `ActionInstance`, `ActionState`, `DeviceInfo`, `Profile`

### Context System

A `Context` identifies a button/encoder position:
```rust
struct Context {
    device: String,    // Device vendor prefix and serial number
    profile: String,   // Profile name
    controller: String, // "Keypad" or "Encoder"
    position: u8,      // Key index or encoder number
}
```

An `ActionContext` extends this with an action instance index for nested actions (e.g., multi-actions).

### State Management

- **Backend**:
  - `DEVICES` (DashMap): Thread-safe device registry, keyed by device ID
  - `CATEGORIES` (RwLock): Plugin actions organized by category for UI
  - `Store<T>`: Generic JSON persistence with file locking, backup, and atomic writes
  - Profile locks: Use `acquire_locks()` (read) or `acquire_locks_mut()` (write) before accessing profiles
- **UI hosts**:
  - `riverdeck-egui`: native egui app; listens for `ui::UiEvent` broadcast hints and pulls state from core singletons
  - `riverdeck-pi`: separate wry/tao process that hosts plugin Property Inspectors in an `<iframe>` and bridges messaging (`postMessage` ↔ IPC) for compatibility helpers
- **Persistence**: JSON files in config dir (see `store/mod.rs`), with `.temp` and `.bak` for crash recovery

### Plugin Communication

- **WebSocket protocol**: Plugins/PIs connect to `localhost:PORT_BASE`, send JSON messages with `event` field
- **Message routing**: `inbound::InboundEventType` enum handles all incoming events, `outbound::` modules send to plugins
- **Outbound event types**: `willAppear`, `keyDown`, `keyUp`, `dialRotate`, etc. (Stream Deck SDK compatible)
- **Authentication**: Context validation ensures plugins can only access their own action instances
- **Plugin manifests** (`manifest.json`: Stream Deck SDK format + extensions):
  - `CodePathLin`: Linux binary path
  - `CodePaths`: Map of Rust target triples to binaries
  - Platform overrides: `manifest.{os}.json` files merged via `json-patch`
- **Property inspectors**: Communicate with plugins via `sendToPlugin`/`sendToPropertyInspector`

### Cross-Platform Considerations

- **Wine support**: Plugins compiled for Windows can run on Linux/macOS via Wine (spawned as child processes)
- **Device access**: Linux requires udev rules (`40-streamdeck.rules`), installed automatically with .deb/.rpm
- **Flatpak**: Special handling for paths (`is_flatpak()` checks), Wine must be installed natively

## Common Patterns

### Adding a New API function

Add it under `crates/riverdeck-core/src/api/` and call it directly from `crates/riverdeck-egui`.

### Profile Management

Profiles are device-specific JSON files in `<config_dir>/<device_id>/<profile_name>.json`:
```rust
// Read profile
let locks = crate::store::profiles::acquire_locks().await;
let profile = locks.profile_stores.get_profile_store(&device, "Default")?;

// Modify profile
let mut locks = crate::store::profiles::acquire_locks_mut().await;
let slot = crate::store::profiles::get_slot_mut(&context, &mut locks).await?;
*slot = Some(new_instance);
crate::store::profiles::save_profile(&device.id, &mut locks).await?;
```

Auto-switching: `application_watcher.rs` polls active window every 250ms, triggers profile changes via `SwitchProfileEvent` emitted to frontend.

### Event Flow Examples

**Button press**: `elgato.rs` → `outbound::keypad::key_down()` → WebSocket → Plugin's `key_down` handler
**Set image**: Plugin sends `setImage` → `inbound::states::set_image()` → `elgato::update_image()` → Device hardware
**Property inspector**: User edits in iframe → `sendToPlugin` → Plugin updates → `setSettings` → Profile saved

## Integration Points

### External Dependencies

- `elgato-streamdeck`: Async hardware communication via HID, image format conversion for different device types
- (No Tauri dependency: core is UI-framework agnostic via `webview::WebviewHost`)
- `tokio-tungstenite`: WebSocket server for plugin communication
- `tiny_http`: Static file server for plugin assets (icons, property inspectors)
- `image`: Image loading/manipulation, format conversion for device displays
- `enigo`: Keyboard/mouse input simulation (starter pack plugin)
- `active-win-pos-rs`: Detect focused application for profile switching (polls every 250ms)
- `sysinfo`: Process monitoring for ApplicationsToMonitor feature

### WebSocket Protocol

Port allocation: `PORT_BASE` (WebSocket), `PORT_BASE + 2` (HTTP static files)
- Dynamic port selection: Tries ports starting at 57116 until both WebSocket and HTTP ports are available
- Registration: Plugins send `RegisterEvent::RegisterPlugin { uuid }`, property inspectors send `RegisterPropertyInspector`
- Message queuing: `PLUGIN_QUEUES` buffers messages until plugin connects
- Separate socket collections: `PLUGIN_SOCKETS` and `PROPERTY_INSPECTOR_SOCKETS` (HashMap of uuid → WebSocket sink)
- Plugin lifecycle: Socket registered → messages processed → socket removed on disconnect

### File Locations

```
Config: ~/.config/io.github.sulrwin.riverdeck/ (Linux) / ~/Library/Application Support/io.github.sulrwin.riverdeck/ (macOS)
Logs:   ~/.local/share/io.github.sulrwin.riverdeck/logs/ (Linux) / ~/Library/Logs/io.github.sulrwin.riverdeck/ (macOS)
Plugins: <config_dir>/plugins/
```

Flatpak uses different paths with `~/.var/app/io.github.sulrwin.riverdeck/` prefix.

## Testing & Debugging

- Run from terminal to see live logs: `cargo run -p riverdeck-egui`
- Plugin logs: Check `<log_dir>/plugins/<uuid>.log` (stdout/stderr captured from plugin processes)
- Debug logging: Uses Rust `log` crate (`log::debug!`)
- Property inspector debugging: devtools support depends on the platform WebView backend (`riverdeck-pi`). For layout/JS debugging you can also open the PI HTML via the local plugin webserver in a browser (note: it will not receive the host `postMessage` connect flow, so it may not connect to RiverDeck)
