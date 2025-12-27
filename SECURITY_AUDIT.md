# RiverDeck Security Audit (Implementation Notes)

This document summarizes the highest-signal security risks found during a full-workspace review and the concrete mitigations implemented in this repo.

## Threat model (balanced)

- **Untrusted plugins / property inspectors**: treat plugin code and HTML inspectors as potentially malicious.
- **Local attacker**: another local user/process attempts to talk to RiverDeck’s localhost IPC endpoints or clobber config files.
- **Supply-chain**: installer + release download integrity.

## High-severity fixes implemented

### 1) Plugin IPC was exposed to the LAN

- **Issue**: WebSocket + static HTTP servers were binding to `0.0.0.0`, exposing the plugin control plane off-host.
- **Fix**: Bind both servers to **`127.0.0.1`** and select ports using loopback-only binds.

Touched:
- `crates/riverdeck-core/src/plugins/mod.rs`
- `crates/riverdeck-core/src/plugins/webserver.rs`

### 2) WebSocket handshake could panic (DoS)

- **Issue**: `unwrap()` usage during first-message registration allowed a client to crash the server task.
- **Fix**: Robust parsing with early returns; no panics on malformed/closed connections.

Touched:
- `crates/riverdeck-core/src/plugins/mod.rs`

### 3) Unbounded WS payload sizes

- **Issue**: A large text message could cause memory/CPU exhaustion.
- **Fix**: Apply a **max message/frame size** using tungstenite config (kept large enough for `setImage` data URLs).

Touched:
- `crates/riverdeck-core/src/plugins/mod.rs`

### 4) Static plugin webserver had permissive CORS + weak path checks

- **Issue**: `Access-Control-Allow-Origin: *` plus weak path prefix checks risked local file disclosure in some modes and made browser-based attacks easier.
- **Fix**:
  - Canonicalize requested paths and ensure they stay under the allowed prefix.
  - Replace wildcard CORS with a conservative allowlist (`localhost` / `127.0.0.1` / Tauri origins).
  - Handle `OPTIONS` preflight.
  - Tighten `postMessage` usage using `document.referrer` origin when available.

Touched:
- `crates/riverdeck-core/src/plugins/webserver.rs`

### 5) Property inspectors had no meaningful authentication

- **Issue**: Any local process could register as a property inspector and push settings for any action context it can guess.
- **Fix (best-effort)**:
  - Add a per-process token in core.
  - Inject a JS shim into PI HTML to send a `riverdeckAuth` message after registration.
  - Require PI sockets to authenticate before accepting PI events (configurable escape hatch: `RIVERDECK_PI_ALLOW_UNAUTH=1`).

Limitations:
- This **does not protect against malware running as the same OS user** (it can generally introspect processes). It *does* raise the bar against other local users and accidental cross-app access.

Touched:
- `crates/riverdeck-core/src/plugins/mod.rs`
- `crates/riverdeck-core/src/plugins/webserver.rs`
- `crates/riverdeck-core/src/events/inbound/mod.rs`
- `crates/riverdeck-core/src/events/mod.rs`

### 6) Malformed inbound plugin messages could crash RiverDeck

- **Issue**: `setTitle` / `setImage` indexed states without bounds checks.
- **Fix**: Bounds checks + safe fallbacks (no panics).

Touched:
- `crates/riverdeck-core/src/events/inbound/states.rs`

### 7) `openUrl` accepted arbitrary schemes

- **Issue**: Untrusted plugin/PI could request opening `file://`, `mailto:`, etc.
- **Fix**: Restrict to **http/https** and cap length.

Touched:
- `crates/riverdeck-core/src/events/inbound/misc.rs`
- `crates/riverdeck-pi/src/main.rs`

### 8) Plugins could fake hardware input events

- **Issue**: Inbound events included `KeyDown`/`KeyUp`/encoder events, allowing a plugin to simulate user input to other plugins.
- **Fix**: Reject those inbound events unless explicitly coming from the internal pathway.

Touched:
- `crates/riverdeck-core/src/events/inbound/mod.rs`

## Medium-severity fixes implemented

### 9) Plugin install zip extraction lacked zip-bomb constraints and was not isolated

- **Issue**: Extraction had no caps on file count / total size. Plugin install extracted directly into the live plugins directory.
- **Fix**:
  - Add file-count and uncompressed-size caps to extraction.
  - Extract into a temp directory and atomically move the plugin folder into place.

Touched:
- `crates/riverdeck-core/src/zip_extract.rs`
- `crates/riverdeck-core/src/api/plugins.rs`

### 10) JSON store should not follow symlinks (best-effort clobber protection)

- **Issue**: Reading/writing store files via symlinks can lead to unintended overwrites in some threat models.
- **Fix**: Refuse to read/write symlinked store/temp/backup files.

Touched:
- `crates/riverdeck-core/src/store/mod.rs`

### 11) Profile ID path traversal

- **Issue**: Profile IDs are used to form file paths; allowing `..` can escape expected directories.
- **Fix**: Validate profile IDs (allow folder-like names, reject `..`, absolute/root/prefix components).

Touched:
- `crates/riverdeck-core/src/store/profiles.rs`

### 12) Plugin directory symlink handling

- **Issue**: Several directory walkers used `metadata().is_symlink()` which can miss symlinks (because `metadata()` follows).
- **Fix**: Use `symlink_metadata` and only follow symlinks that remain under the plugins directory.

Touched:
- `crates/riverdeck-core/src/plugins/mod.rs`
- `crates/riverdeck-core/src/api/plugins.rs`
- `crates/riverdeck-core/src/events/outbound/mod.rs`

## Installer / supply-chain hardening

### 13) `install_riverdeck.sh` robustness and integrity

- **Fixes**:
  - Prefer `jq` parsing for GitHub release assets (fallback kept).
  - Stronger curl defaults (`--fail`, TLS constraints).
  - Best-effort SHA256 verification (when a checksum asset exists) and explicit confirmation when it cannot be verified.
  - Pin udev rules fetch to the **latest release tag** when possible (fallback remains).
  - Make unsigned RPM installs an explicit opt-in via prompts.

Touched:
- `install_riverdeck.sh`

## Verification performed

- `cargo fmt --all`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test -p riverdeck-core`

## Dependency auditing (RustSec)

`cargo audit` is not available in this environment by default. To run it:

```bash
cargo install cargo-audit
cargo audit
```


