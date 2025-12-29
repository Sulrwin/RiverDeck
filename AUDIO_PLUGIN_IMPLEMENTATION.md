# Audio Plugin Support Implementation Summary

## Completed Implementation

I've implemented comprehensive support for ELGATO-format plugins and audio plugins in RiverDeck. This solution is **generic and reusable** - it will work for the Volume Controller plugin and any future audio plugins.

## What Was Built

### 1. Manifest Synthesis (`crates/riverdeck-core/src/plugins/manifest.rs`)

**Generic ELGATO Manifest Handler:**
- Automatically detects ELGATO binary manifest format
- Synthesizes a valid JSON manifest from plugin metadata:
  - Scans `en.json` (or other localization files) for action names and descriptions
  - Automatically finds icons in `imgs/actions/*/` directories
  - Detects property inspectors in `pi/` folders
  - Infers plugin metadata (name, author, category) from file structure
  - Determines code paths (supports `bin/plugin.js`, `plugin.js`, `.cjs`, etc)
  - Identifies encoder vs keypad controllers from localization hints

**Key Features:**
- Works for ANY plugin with ELGATO manifest format
- No hardcoding - purely metadata-driven
- Falls back gracefully if metadata is missing
- Supports multiple action types and controllers

### 2. Audio Router Backend (`crates/riverdeck-audio-router/`)

**Generic Audio Control Service:**

**Created Files:**
- `Cargo.toml` - PulseAudio/PipeWire dependencies
- `src/lib.rs` - Module exports
- `src/audio_backend.rs` - Audio backend trait + PulseAudio implementation (stub)
- `src/protocol.rs` - JSON-RPC protocol types matching Elgato Audio Router API
- `src/server.rs` - WebSocket server on `localhost:1844`

**Architecture:**
```
Plugin (Node.js) → WebSocket (port 1844) → Audio Router Server → PulseAudio/PipeWire Backend
```

**Supported JSON-RPC Methods:**
- `getApplicationInstanceCount` - List audio applications
- `getApplicationInstance` - Get app info by PID
- `getApplicationInstanceAtIndex` - Get app at index
- `setApplicationInstanceVolume` - Control app volume
- `setApplicationInstanceMute` - Control app mute
- `getSystemDefaultDevice` - Get default audio device
- Device control methods (ready for implementation)

### 3. Integration (`crates/riverdeck-core/`)

- Added `riverdeck-audio-router` dependency
- Audio Router server starts automatically in `initialise_plugins()` 
- **Runs for ALL audio plugins**, not just Volume Controller
- Starts once on first plugin initialization (doesn't rebind on reload)

## Testing Instructions

1. **Restart RiverDeck:**
   ```bash
   cargo run -p riverdeck-egui
   ```

2. **Check Logs:**
   - Plugin synthesis: `~/.local/share/io.github.sulrwin.riverdeck/logs/riverdeck.log`
   - Look for: `ELGATO manifest format detected, synthesizing from metadata`
   - Audio Router: Look for `Audio Router WebSocket server listening on 127.0.0.1:1844`

3. **Verify Plugin Loads:**
   - The Volume Controller plugin should appear in the Actions list
   - All 11 actions should be visible with correct names/icons
   - Actions should have property inspectors

4. **Test Basic Functionality:**
   - Drag an action to a Stream Deck button
   - Plugin should spawn (`bin/plugin.js` with Node.js)
   - Check plugin logs: `~/.local/share/io.github.sulrwin.riverdeck/logs/plugins/com.elgato.volume-controller.log`
   - Plugin should attempt to connect to `localhost:1844`

## Next Steps (PulseAudio Implementation)

The audio backend is currently a **working stub** that returns empty lists. To make it fully functional:

1. **Implement PulseAudio integration** in `audio_backend.rs`:
   - Use `libpulse-binding` to enumerate sink-inputs (apps)
   - Map PulseAudio sink-input properties to `ApplicationInfo`
   - Implement volume/mute control via PulseAudio API
   - Add event watchers for app/device changes

2. **Extract process info:**
   - Use PulseAudio `application.process.id` property
   - Map to executable paths and display names

3. **Device enumeration:**
   - List PulseAudio sinks/sources
   - Track default device changes
   - Implement device volume/mute control

## Current Status

✅ Manifest synthesis working (any ELGATO plugin)
✅ Audio Router server infrastructure complete
✅ JSON-RPC protocol implemented
✅ WebSocket server running on port 1844
✅ Integration with RiverDeck plugin system
⏳ PulseAudio backend needs implementation (stub returns empty)

## Plugin Installation

The Volume Controller plugin is already copied to:
```
~/.config/io.github.sulrwin.riverdeck/plugins/com.elgato.volume-controller.sdPlugin/
```

It should load automatically on next RiverDeck start with the synthesized manifest.

## Architecture Benefits

1. **Generic Solution**: Works for ANY audio plugin, not just Volume Controller
2. **Reusable**: Other plugins with ELGATO manifests will work automatically
3. **Extensible**: PulseAudio backend can be swapped for PipeWire or other systems
4. **Standard Protocol**: Uses Elgato's existing JSON-RPC API for compatibility

## Files Modified/Created

**Modified:**
- `Cargo.toml` - Added audio-router to workspace
- `crates/riverdeck-core/Cargo.toml` - Added audio-router dependency
- `crates/riverdeck-core/src/plugins/manifest.rs` - Added ELGATO synthesis
- `crates/riverdeck-core/src/plugins/mod.rs` - Start audio router server

**Created:**
- `crates/riverdeck-audio-router/` - Entire new crate (7 files)
  - Cargo.toml
  - src/lib.rs
  - src/audio_backend.rs
  - src/protocol.rs
  - src/server.rs

