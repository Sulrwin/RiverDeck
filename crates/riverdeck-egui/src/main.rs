use riverdeck_core::{application_watcher, elgato, plugins, shared, ui};

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::Duration;
use std::{
    fs,
    fs::OpenOptions,
    path::{Path, PathBuf},
    sync::Mutex,
};

use fs2::FileExt;
use log::LevelFilter;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::runtime::Runtime;
use tokio::sync::broadcast;
use tokio::sync::mpsc as tokio_mpsc;

#[cfg(feature = "tray")]
use std::sync::atomic::AtomicBool;

#[cfg(any(unix, feature = "tray"))]
use std::sync::atomic::Ordering;

#[cfg(feature = "tray")]
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
#[cfg(feature = "tray")]
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

#[cfg(feature = "tray")]
static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
static SHOW_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

static IPC_PORT: OnceLock<u16> = OnceLock::new();

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum IpcRequest {
    Show,
    DeepLink { url: String },
    MarketplaceToken { token: String },
    MarketplacePing { href: String },
}

#[derive(Debug, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum IpcResponse {
    Ok,
    Err { error: String },
}

struct TeeLogger {
    stderr: env_logger::Logger,
    file: Option<std::sync::Mutex<std::fs::File>>,
}

impl log::Log for TeeLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        self.stderr.enabled(metadata)
    }

    fn log(&self, record: &log::Record<'_>) {
        self.stderr.log(record);
        let Some(file) = self.file.as_ref() else {
            return;
        };
        // Best-effort: never let logging failures affect app runtime.
        let mut file = file.lock().unwrap_or_else(|p| p.into_inner());
        let _ = writeln!(
            file,
            "{:?} {:<5} {} - {}",
            std::time::SystemTime::now(),
            record.level(),
            record.target(),
            record.args()
        );
        let _ = file.flush();
    }

    fn flush(&self) {
        self.stderr.flush();
        if let Some(file) = self.file.as_ref() {
            let mut file = file.lock().unwrap_or_else(|p| p.into_inner());
            let _ = file.flush();
        }
    }
}

fn init_logging() {
    // Configure and install a logger:
    // - still respects RUST_LOG (useful for debugging)
    // - always writes a persistent log file for GUI launches where stdout/stderr is invisible
    //
    // NOTE: This must run after `shared::init_paths()` so `shared::log_dir()` is available.
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));

    // If the user didn't specify max log level explicitly, avoid dependency spam by default.
    // (They can always `RUST_LOG=trace`.)
    if std::env::var("RUST_LOG").is_err() {
        builder.filter_level(LevelFilter::Info);
    }

    let stderr_logger = builder.build();

    let file = (|| {
        // Primary: XDG data dir logs (e.g. ~/.local/share/.../logs)
        let dir = shared::log_dir();
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("riverdeck-egui.log");
        if let Ok(f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            return Some(std::sync::Mutex::new(f));
        }

        // Fallback: config dir (e.g. ~/.config/...) so we always get *some* file even if the
        // XDG data dir is missing/unwritable.
        let cfg = shared::config_dir();
        let _ = std::fs::create_dir_all(&cfg);
        let cfg_path = cfg.join("riverdeck-egui.log");
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(cfg_path)
            .ok()?;
        Some(std::sync::Mutex::new(f))
    })();

    let _ = log::set_boxed_logger(Box::new(TeeLogger {
        stderr: stderr_logger,
        file,
    }));
    // Let the inner logger handle filtering; keep max_level permissive.
    log::set_max_level(LevelFilter::Trace);
}

fn env_truthy_any(name: &str) -> bool {
    matches!(
        std::env::var(name).ok().as_deref(),
        Some("1")
            | Some("true")
            | Some("TRUE")
            | Some("yes")
            | Some("YES")
            | Some("on")
            | Some("ON")
    )
}

fn main() -> anyhow::Result<()> {
    // Development quality-of-life: when running under an IDE/debugger, it's easy to end up with an
    // orphaned `riverdeck` process if the parent launcher is force-killed.
    //
    // On Linux in debug builds, request SIGTERM when our parent dies.
    #[cfg(target_os = "linux")]
    set_parent_death_signal();

    let args: Vec<String> = std::env::args().collect();
    let start_hidden = args.iter().any(|a| a == "--hide");
    let replace_instance = args.iter().any(|a| a == "--replace");
    let show_existing = args.iter().any(|a| a == "--show");
    let deep_links: Vec<String> = args
        .iter()
        .filter(|a| is_deep_link_arg(a))
        .cloned()
        .collect();
    let has_deep_links = !deep_links.is_empty();
    let spawn_all_devices = args.iter().any(|a| a == "--spawn-all-devices");
    if spawn_all_devices {
        // Dev/testing mode: register mock devices for each supported Stream Deck family so the UI
        // can be visually verified without hardware attached.
        // SAFETY: we are in `main()` before any worker threads are spawned, so mutating the
        // process environment cannot race other threads reading it.
        unsafe {
            std::env::set_var("RIVERDECK_TEST_DEVICES", "all");
        }
    }

    let mut paths = shared::discover_paths()?;
    // Best-effort resource dir discovery:
    // - macOS .app: `<App>.app/Contents/Resources/plugins`
    // - other packaged: `<exe_dir>/plugins`
    // - dev: repo root `plugins/`
    if let Ok(exe) = std::env::current_exe()
        && let Some(exe_dir) = exe.parent()
    {
        let mac_resources = exe_dir.join("../Resources");
        if looks_like_bundled_resources(&mac_resources) {
            paths.resource_dir = Some(mac_resources);
        } else if !cfg!(debug_assertions) && looks_like_bundled_resources(exe_dir) {
            paths.resource_dir = Some(exe_dir.to_path_buf());
        }
    }
    if paths.resource_dir.is_none() {
        let dev_resource_dir = std::env::current_dir().ok();
        if dev_resource_dir
            .as_ref()
            .is_some_and(|d| looks_like_bundled_resources(d))
        {
            paths.resource_dir = dev_resource_dir;
        }
    }
    shared::init_paths(paths);

    // Initialize logging after paths so we can write to the persistent log directory.
    init_logging();
    if riverdeck_core::marketplace::load_marketplace_access_token_from_disk() {
        log::info!("Loaded marketplace token from disk (best-effort)");
    }
    log::info!(
        "RiverDeck starting (pid={}, args={:?})",
        std::process::id(),
        args
    );
    log::info!("config_dir={}", shared::config_dir().display());
    log::info!("log_dir={}", shared::log_dir().display());
    log::info!(
        "resource_dir={}",
        shared::resource_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<none>".to_owned())
    );

    configure_autostart();

    // Single-instance: lockfile + PID. In dev/debug, Cursor/VScode sometimes kills the `cargo`
    // parent but leaves the spawned `riverdeck` binary running, so we support a best-effort
    // takeover (`--replace`) to make restarts reliable.
    std::fs::create_dir_all(shared::config_dir())?;
    // Ensure placeholder icons exist for core built-in actions in dev/debug (and as a fallback).
    shared::ensure_builtin_icons();
    let mut lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(shared::config_dir().join("riverdeck.lock"))?;
    if lock_file.try_lock_exclusive().is_err() {
        // Another instance is running.
        // In debug builds we often want takeover for fast restarts, but deep-link invocations should
        // forward to the existing instance instead of killing it.
        let should_replace = replace_instance
            || (!has_deep_links
                && (cfg!(debug_assertions)
                    || std::env::var("RIVERDECK_REPLACE_INSTANCE").ok().as_deref() == Some("1")));

        if should_replace {
            let pid = read_lock_pid(&mut lock_file);
            if let Some(pid) = pid {
                #[cfg(unix)]
                {
                    if pid > 0 && pid != std::process::id() {
                        // Linux safety: only kill if the locked PID still points at a riverdeck binary.
                        #[cfg(target_os = "linux")]
                        if !linux_pid_looks_like_riverdeck(pid) {
                            log::warn!(
                                "Lockfile held (pid {pid}), but it doesn't look like a RiverDeck process; not replacing instance"
                            );
                        } else {
                            log::warn!("Replacing existing RiverDeck instance (pid {pid})");
                            terminate_and_wait_for_lock_release(pid, &lock_file);
                        }
                        #[cfg(not(target_os = "linux"))]
                        {
                            log::warn!("Replacing existing RiverDeck instance (pid {pid})");
                            terminate_and_wait_for_lock_release(pid, &lock_file);
                        }
                    }
                }
            } else {
                log::warn!("Lockfile held, but failed to read PID; cannot replace instance");
            }

            // Retry after attempting replacement.
            if lock_file.try_lock_exclusive().is_err() {
                log::warn!(
                    "RiverDeck already running; could not acquire lock after replacement attempt"
                );
                return Ok(());
            }
        } else {
            if has_deep_links
                && let Some(port) = read_lock_ipc_port(&mut lock_file)
                && forward_ipc_deep_links(port, &deep_links).is_ok()
            {
                return Ok(());
            }

            // Best-effort UX: ask the running instance to show its window, so the user doesn't
            // end up with an invisible background process (e.g. hidden-to-tray with no tray).
            let pid = read_lock_pid(&mut lock_file);
            #[cfg(unix)]
            if let Some(pid) = pid {
                #[cfg(target_os = "linux")]
                if linux_pid_looks_like_riverdeck(pid) {
                    request_show_existing_instance(pid);
                }
                #[cfg(not(target_os = "linux"))]
                request_show_existing_instance(pid);
            }

            if show_existing {
                log::info!("Requested existing RiverDeck instance to show its window");
            } else {
                log::warn!(
                    "RiverDeck already running (lockfile held). Pass `--replace` to take over, or `--show` to request the existing window."
                );
            }
            return Ok(());
        }
    }

    // If a previous instance was force-killed, it may have left plugin/PI subprocesses behind.
    // Reap those before we start new servers or spawn new plugins.
    riverdeck_core::lifecycle::startup_cleanup();

    let (ui_tx, _ui_rx) = broadcast::channel(256);
    ui::init(ui_tx);

    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("riverdeck-core")
            .build()?,
    );

    // Start a small local IPC server so secondary invocations (e.g. deep links) can forward
    // requests to this running instance.
    let ipc_port = runtime.block_on(start_ipc_server()).ok();
    if let Some(port) = ipc_port {
        let _ = IPC_PORT.set(port);
    }
    // Record our PID (+ IPC port when available) into the lock file (best-effort).
    let _ = write_lock_info(&mut lock_file, ipc_port);

    start_signal_handlers(runtime.clone());
    start_core_background(runtime.clone());

    let update_info: Arc<Mutex<Option<UpdateInfo>>> = Arc::new(Mutex::new(None));
    start_update_check(runtime.clone(), update_info.clone());
    handle_startup_args(runtime.clone(), args.clone());

    // Exit the process when the main window is closed.
    // This prevents "background instances" that keep running after the window is gone.
    log::info!("Preparing native window options");
    let mut native_options = eframe::NativeOptions {
        run_and_return: false,
        ..Default::default()
    };
    // On Linux, window button placement (left/right) is usually controlled by the window manager.
    // If we want "always top-right" regardless of WM settings, we need a custom title bar.
    //
    // NOTE: This intentionally only applies to Linux to avoid fighting platform conventions
    // (macOS uses top-left window controls).
    #[cfg(target_os = "linux")]
    {
        native_options.viewport = native_options.viewport.with_decorations(false);
    }
    log::info!("Loading window icon (best-effort)");
    // SAFETY VALVE: some environments have shown hangs during icon decode in debug builds.
    // The window icon is non-critical; default to skipping it in debug builds to ensure the UI
    // always starts. Set `RIVERDECK_ENABLE_WINDOW_ICON=1` to force-enable.
    let enable_window_icon = env_truthy_any("RIVERDECK_ENABLE_WINDOW_ICON")
        && !env_truthy_any("RIVERDECK_DISABLE_WINDOW_ICON");
    let icon = if enable_window_icon {
        Some(load_window_icon_with_timeout(Duration::from_millis(250)))
    } else {
        None
    }
    .flatten();

    #[cfg(debug_assertions)]
    if !enable_window_icon {
        log::warn!(
            "Window icon disabled (debug build safety); set RIVERDECK_ENABLE_WINDOW_ICON=1 to enable"
        );
    }

    if let Some(icon) = icon {
        native_options.viewport = native_options.viewport.with_icon(icon);
    }
    log::info!("Starting UI: entering eframe::run_native()");
    eframe::run_native(
        "RiverDeck",
        native_options,
        Box::new(move |cc| {
            log::info!("eframe callback invoked: constructing RiverDeckApp");
            Ok(Box::new(RiverDeckApp::new(
                cc,
                runtime,
                lock_file,
                start_hidden,
                update_info,
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    Ok(())
}

#[cfg(unix)]
fn start_signal_handlers(runtime: Arc<Runtime>) {
    runtime.spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};

        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut sigusr1 = match signal(SignalKind::user_defined1()) {
            Ok(s) => s,
            Err(_) => return,
        };

        loop {
            tokio::select! {
                _ = sigusr1.recv() => {
                    SHOW_REQUESTED.store(true, Ordering::SeqCst);
                },
                _ = sigterm.recv() => {
                    log::warn!("Received termination signal; shutting down RiverDeck");
                    riverdeck_core::lifecycle::shutdown_all().await;
                    std::process::exit(0);
                },
                _ = sigint.recv() => {
                    log::warn!("Received interrupt signal; shutting down RiverDeck");
                    riverdeck_core::lifecycle::shutdown_all().await;
                    std::process::exit(0);
                },
            }
        }
    });
}

#[cfg(not(unix))]
fn start_signal_handlers(_runtime: Arc<Runtime>) {}

#[cfg(feature = "tray")]
fn env_truthy(name: &str) -> bool {
    matches!(
        std::env::var(name).ok().as_deref(),
        Some("1")
            | Some("true")
            | Some("TRUE")
            | Some("yes")
            | Some("YES")
            | Some("on")
            | Some("ON")
    )
}

/// Best-effort heuristic for whether "hide to tray" is a safe UX.
///
/// Motivation: on some desktops (notably GNOME without an AppIndicator extension), the tray icon
/// may not appear even if `tray-icon` succeeds in creating it. In that case, hiding the window
/// creates an invisible background process with no obvious way to restore it.
#[cfg(feature = "tray")]
fn tray_hide_is_safe_by_default() -> bool {
    if env_truthy("RIVERDECK_DISABLE_TRAY") {
        return false;
    }
    if env_truthy("RIVERDECK_FORCE_TRAY") {
        return true;
    }

    #[cfg(target_os = "linux")]
    {
        let desktop = std::env::var("XDG_CURRENT_DESKTOP")
            .or_else(|_| std::env::var("XDG_SESSION_DESKTOP"))
            .unwrap_or_default()
            .to_lowercase();

        // GNOME generally does not display StatusNotifier/AppIndicator icons by default.
        // Users can opt-in via `RIVERDECK_FORCE_TRAY=1`.
        if desktop.contains("gnome") {
            return false;
        }
    }

    true
}

fn load_window_icon() -> anyhow::Result<egui::IconData> {
    let (rgba, width, height) = load_embedded_logo_rgba()?;
    Ok(egui::IconData {
        rgba,
        width,
        height,
    })
}

fn load_window_icon_with_timeout(timeout: Duration) -> Option<egui::IconData> {
    // Safety valve: some users have observed startup hangs during PNG decode in debug builds.
    // The window icon is non-critical, so we load it off-thread and proceed if it takes too long.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(load_window_icon());
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(icon)) => Some(icon),
        Ok(Err(err)) => {
            log::warn!("Window icon load failed: {err:#}");
            None
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            log::warn!(
                "Window icon load timed out after {:?}; continuing without icon",
                timeout
            );
            None
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => None,
    }
}

fn load_embedded_logo_rgba() -> anyhow::Result<(Vec<u8>, u32, u32)> {
    // Embed the icon so it doesn't depend on the current working directory.
    let bytes = include_bytes!("../../../packaging/icons/icon.png");
    let img = image::load_from_memory(bytes)?.into_rgba8();
    let (w, h) = (img.width(), img.height());
    Ok((img.into_raw(), w, h))
}

#[cfg(target_os = "linux")]
fn set_parent_death_signal() {
    // Only enable in debug builds (dev runs from Cursor/VSCode).
    if !cfg!(debug_assertions) {
        return;
    }

    // SAFETY: `prctl` is a C ABI call. If it fails, we ignore it (best-effort).
    unsafe {
        let _ = libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
    }
}

fn read_lock_pid(lock_file: &mut std::fs::File) -> Option<u32> {
    let mut buf = String::new();
    let _ = lock_file.seek(SeekFrom::Start(0));
    let mut take = lock_file.take(64);
    if take.read_to_string(&mut buf).is_err() {
        return None;
    }
    buf.lines().next()?.trim().parse::<u32>().ok()
}

fn write_lock_info(lock_file: &mut std::fs::File, ipc_port: Option<u16>) -> std::io::Result<()> {
    lock_file.set_len(0)?;
    lock_file.seek(SeekFrom::Start(0))?;
    writeln!(lock_file, "{}", std::process::id())?;
    if let Some(port) = ipc_port {
        writeln!(lock_file, "{port}")?;
    }
    lock_file.sync_all()?;
    Ok(())
}

fn read_lock_ipc_port(lock_file: &mut std::fs::File) -> Option<u16> {
    let mut buf = String::new();
    let _ = lock_file.seek(SeekFrom::Start(0));
    let mut take = lock_file.take(64);
    if take.read_to_string(&mut buf).is_err() {
        return None;
    }
    buf.lines().nth(1)?.trim().parse::<u16>().ok()
}

fn is_deep_link_arg(arg: &str) -> bool {
    arg.starts_with("openaction://")
        || arg.starts_with("opendeck://")
        || arg.starts_with("streamdeck://")
        || arg.starts_with("riverdeck://")
}

async fn start_ipc_server() -> anyhow::Result<u16> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        loop {
            let (stream, _addr) = match listener.accept().await {
                Ok(x) => x,
                Err(_) => break,
            };
            tokio::spawn(async move {
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = tokio::io::BufReader::new(read_half);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(_) => break,
                    }
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let req = match serde_json::from_str::<IpcRequest>(trimmed) {
                        Ok(r) => r,
                        Err(err) => {
                            let _ = write_half_json(
                                &mut write_half,
                                IpcResponse::Err {
                                    error: format!("invalid request: {err:#}"),
                                },
                            )
                            .await;
                            continue;
                        }
                    };

                    let res: anyhow::Result<()> = match req {
                        IpcRequest::Show => {
                            #[cfg(unix)]
                            {
                                SHOW_REQUESTED.store(true, Ordering::SeqCst);
                            }
                            Ok(())
                        }
                        IpcRequest::DeepLink { url } => {
                            log::info!("IPC deep link received: {url}");
                            match handle_deep_link_arg(&url).await {
                                Ok(()) => Ok(()),
                                Err(err) => {
                                    log::warn!("IPC deep link handling failed: {err:#}");
                                    Err(err)
                                }
                            }
                        }
                        IpcRequest::MarketplaceToken { token } => {
                            log::info!(
                                "Marketplace token received via IPC (len={})",
                                token.trim().len()
                            );
                            riverdeck_core::marketplace::set_marketplace_access_token(token);
                            Ok(())
                        }
                        IpcRequest::MarketplacePing { href } => {
                            log::info!("Marketplace IPC ping received (href={href})");
                            Ok(())
                        }
                    };

                    let _ = match res {
                        Ok(()) => write_half_json(&mut write_half, IpcResponse::Ok).await,
                        Err(err) => {
                            write_half_json(
                                &mut write_half,
                                IpcResponse::Err {
                                    error: format!("{err:#}"),
                                },
                            )
                            .await
                        }
                    };
                }
            });
        }
    });

    Ok(port)
}

async fn write_half_json(
    w: &mut tokio::net::tcp::OwnedWriteHalf,
    resp: IpcResponse,
) -> std::io::Result<()> {
    let mut s = serde_json::to_string(&resp)
        .unwrap_or_else(|_| "{\"type\":\"err\",\"error\":\"serialize\"}".to_owned());
    s.push('\n');
    w.write_all(s.as_bytes()).await?;
    Ok(())
}

fn forward_ipc_deep_links(port: u16, deep_links: &[String]) -> anyhow::Result<()> {
    use std::net::{SocketAddr, TcpStream};

    let addr: SocketAddr = format!("127.0.0.1:{port}").parse()?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(300))?;
    let _ = stream.set_write_timeout(Some(Duration::from_millis(300)));
    let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));

    fn send(stream: &mut TcpStream, req: &IpcRequest) -> std::io::Result<()> {
        let mut s = serde_json::to_string(req).unwrap_or_else(|_| "{\"type\":\"show\"}".to_owned());
        s.push('\n');
        stream.write_all(s.as_bytes())
    }

    // Best-effort: show the existing window for UX.
    let _ = send(&mut stream, &IpcRequest::Show);
    for url in deep_links {
        let _ = send(&mut stream, &IpcRequest::DeepLink { url: url.clone() });
    }
    Ok(())
}

#[cfg(all(unix, target_os = "linux"))]
fn linux_pid_looks_like_riverdeck(pid: u32) -> bool {
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok();
    let Some(exe) = exe else {
        return false;
    };
    let Some(name) = exe.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    // When running from `cargo run` and recompiling, Linux can show the process exe as
    // `riverdeck (deleted)` (the inode is unlinked). Treat that as a RiverDeck process too.
    let name = name.strip_suffix(" (deleted)").unwrap_or(name);
    name == "riverdeck" || name == "riverdeck-egui" || name == "riverdeck.exe"
}

#[cfg(unix)]
fn terminate_and_wait_for_lock_release(pid: u32, lock_file: &std::fs::File) {
    // Try SIGTERM first.
    let _ = signal_pid(pid, libc::SIGTERM);
    for _ in 0..20 {
        if lock_file.try_lock_exclusive().is_ok() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // Escalate.
    let _ = signal_pid(pid, libc::SIGKILL);
    for _ in 0..20 {
        if lock_file.try_lock_exclusive().is_ok() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(unix)]
fn signal_pid(pid: u32, sig: i32) -> std::io::Result<()> {
    let pid_i32 = i32::try_from(pid)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "pid out of range"))?;
    let res = unsafe { libc::kill(pid_i32, sig) };
    if res == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    // If process is already gone, treat as success.
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(err)
}

#[cfg(unix)]
fn request_show_existing_instance(pid: u32) {
    let _ = signal_pid(pid, libc::SIGUSR1);
}

fn looks_like_bundled_resources(dir: &Path) -> bool {
    let plugins_dir = dir.join("plugins");
    if !plugins_dir.is_dir() {
        return false;
    }

    // Bundled plugins are expected to be laid out as `plugins/<id>.sdPlugin/manifest.json`.
    // In this repo, source plugins may instead be in `plugins/<id>.sdPlugin/assets/manifest.json`,
    // which should NOT be treated as "bundled resources" (otherwise core logs scary errors).
    let Ok(entries) = fs::read_dir(&plugins_dir) else {
        return false;
    };
    for entry in entries.flatten() {
        if entry.path().join("manifest.json").is_file() {
            return true;
        }
    }
    false
}

fn configure_autostart() {
    let Ok(store) = riverdeck_core::store::get_settings() else {
        return;
    };
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(exe_str) = exe.to_str() else { return };

    let Ok(auto) = auto_launch::AutoLaunchBuilder::new()
        .set_app_name("RiverDeck")
        .set_app_path(exe_str)
        .set_args(&["--hide"])
        .build()
    else {
        return;
    };

    let _ = if store.value.autolaunch {
        auto.enable()
    } else {
        auto.disable()
    };
}

fn handle_startup_args(runtime: Arc<Runtime>, args: Vec<String>) {
    runtime.spawn(async move {
        for arg in &args {
            if arg.starts_with("openaction://")
                || arg.starts_with("opendeck://")
                || arg.starts_with("streamdeck://")
                || arg.starts_with("riverdeck://")
            {
                let _ = handle_deep_link_arg(arg).await;
            } else if arg == "--reload-plugin" {
                // handled below via positional scan
            }
        }

        // `--reload-plugin <id>` support
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if a == "--reload-plugin"
                && let Some(id) = it.next()
            {
                riverdeck_core::api::plugins::reload_plugin(id.clone()).await;
            }
        }
    });
}

async fn handle_deep_link_arg(arg: &str) -> anyhow::Result<()> {
    let Ok(url) = reqwest::Url::parse(arg) else {
        return Ok(());
    };
    let host = url.host_str().unwrap_or_default();
    let segments: Vec<&str> = url.path_segments().map(|s| s.collect()).unwrap_or_default();
    let kind = segments.first().copied().unwrap_or_default();
    let raw_id = segments.get(1).copied().unwrap_or_default();

    // OpenAction Marketplace (`marketplace.rivul.us`) installs currently trigger OpenDeck-style
    // deep links like `opendeck://installPlugin/<pluginId>`.
    if url.scheme() == "opendeck" && host.eq_ignore_ascii_case("installPlugin") {
        let plugin_id = url
            .path_segments()
            .and_then(|mut s| s.next())
            .unwrap_or_default()
            .trim();
        if plugin_id.is_empty() {
            return Ok(());
        }

        let resolved =
            match riverdeck_core::openaction_marketplace::resolve_install(plugin_id).await {
                Ok(r) => r,
                Err(e) => {
                    let msg = format!("OpenAction marketplace install failed: {e:#}");
                    riverdeck_core::ui::emit(riverdeck_core::ui::UiEvent::PluginInstall {
                        id: plugin_id.to_owned(),
                        phase: riverdeck_core::ui::PluginInstallPhase::Finished {
                            ok: false,
                            error: Some(msg.clone()),
                        },
                    });
                    return Err(anyhow::anyhow!(msg));
                }
            };

        if let Some(download_url) = resolved.download_url.as_deref() {
            log::info!(
                "Installing plugin via OpenAction marketplace: id={}, url={}",
                resolved.plugin_id,
                download_url
            );
            if let Err(err) = riverdeck_core::api::plugins::install_plugin(
                Some(download_url.to_owned()),
                None,
                Some(resolved.plugin_id.clone()),
            )
            .await
            {
                riverdeck_core::ui::emit(riverdeck_core::ui::UiEvent::PluginInstall {
                    id: resolved.plugin_id.clone(),
                    phase: riverdeck_core::ui::PluginInstallPhase::Finished {
                        ok: false,
                        error: Some(format!("{err:#}")),
                    },
                });
                return Err(err);
            }
            riverdeck_core::plugins::initialise_plugins();
            return Ok(());
        }

        // No direct download artifact found. Open the repository (best-effort) and surface a clear
        // message so this doesn't feel like a no-op.
        if let Some(repo) = resolved.repository_url.as_deref() {
            open_url_in_browser(repo);
        }
        riverdeck_core::ui::emit(riverdeck_core::ui::UiEvent::PluginInstall {
            id: resolved.plugin_id.clone(),
            phase: riverdeck_core::ui::PluginInstallPhase::Finished {
                ok: false,
                error: Some(
                    "This OpenAction marketplace entry does not provide an installable download artifact. RiverDeck tried the repo’s GitHub releases, but couldn’t find a `.streamDeckPlugin`/`.zip` asset. Please download the plugin bundle from the repository and install it via Settings → Plugins → Install…"
                        .to_owned(),
                ),
            },
        });
        return Ok(());
    }

    // Plugin deep-link handling (existing behavior).
    //
    // Note: The Elgato marketplace has used multiple path forms over time, e.g.
    // `streamdeck://plugins/message/<id>` and `streamdeck://plugins/install/<id>`.
    // Treat both as "install/route to plugin" requests.
    if host == "plugins" && (kind == "message" || kind == "install") {
        // The Elgato marketplace uses `streamdeck://plugins/message/<id>`.
        // In RiverDeck we represent plugin IDs as `<id>.sdPlugin`.
        let plugin_id = if raw_id.is_empty() {
            String::new()
        } else if url.scheme() == "streamdeck" && !raw_id.ends_with(".sdPlugin") {
            format!("{raw_id}.sdPlugin")
        } else {
            raw_id.to_owned()
        };

        // If plugin is already installed, forward deep link to it (Stream Deck SDK behavior).
        if !plugin_id.is_empty() {
            let installed = shared::config_dir()
                .join("plugins")
                .join(&plugin_id)
                .is_dir();
            if installed {
                let _ = riverdeck_core::events::outbound::deep_link::did_receive_deep_link(
                    &plugin_id,
                    arg.to_owned(),
                )
                .await;
                return Ok(());
            }
        }

        // Not installed: treat as marketplace "install" request.
        if let Some(download_url) = extract_plugin_download_url(&url) {
            let download_l = download_url.to_lowercase();
            let is_icon_pack = download_l.contains(".streamdeckiconpack")
                || download_l.contains("streamdeckiconpack");

            if is_icon_pack {
                log::info!("Installing icon pack via deep link: id={raw_id}, url={download_url}");
                let fallback_id = raw_id.to_owned();
                let _installed_id = riverdeck_core::api::icon_packs::install_icon_pack(
                    download_url,
                    Some(fallback_id),
                )
                .await?;
                return Ok(());
            }

            // Some deep links omit the plugin id in the path. Try to infer a stable-ish fallback id
            // from the download URL, otherwise fall back to an empty id (the installer still works).
            let inferred_fallback_id = reqwest::Url::parse(&download_url)
                .ok()
                .and_then(|u| {
                    u.path_segments()
                        .and_then(|mut s| s.next_back().map(|x| x.to_owned()))
                })
                .unwrap_or_default()
                .trim()
                .trim_end_matches(".streamDeckPlugin")
                .trim_end_matches(".streamdeckplugin")
                .trim_end_matches(".zip")
                .to_owned();
            let fallback_id = if !plugin_id.is_empty() {
                plugin_id
                    .strip_suffix(".sdPlugin")
                    .unwrap_or(plugin_id.as_str())
                    .to_owned()
            } else if !raw_id.is_empty() {
                raw_id.to_owned()
            } else if !inferred_fallback_id.is_empty() {
                inferred_fallback_id
            } else {
                "plugin".to_owned()
            };

            let log_id = if plugin_id.is_empty() {
                fallback_id.as_str()
            } else {
                plugin_id.as_str()
            };
            log::info!("Installing plugin via deep link: id={log_id}, url={download_url}");
            riverdeck_core::api::plugins::install_plugin(
                Some(download_url),
                None,
                Some(fallback_id),
            )
            .await?;
            // Make it available immediately (best-effort).
            riverdeck_core::plugins::initialise_plugins();
            return Ok(());
        }

        // If we couldn't extract a direct download URL, fall back to Elgato-specific resolution
        // for Stream Deck marketplace install links like `streamdeck://plugins/install/<variantId>`.
        //
        // Important: only do this for `streamdeck://...` links so we don't block other ecosystems
        // (e.g. OpenAction/OpenDeck style deep links).
        if kind == "install" && url.scheme() == "streamdeck" && !raw_id.is_empty() {
            // Newer marketplace deep links (`.../install/<uuid>`) often only include a marketplace
            // *variant id* and require a Marketplace session token to resolve into a signed download
            // URL. We capture this token from the Marketplace webview (best-effort) and use it here.
            if let Some(tok) = riverdeck_core::marketplace::marketplace_access_token()
                && let Some(links) = riverdeck_core::marketplace::purchase_link(&tok, raw_id).await
            {
                // Prefer the direct download link. If only a deep link exists, try to extract
                // a download URL from it.
                let candidate = links
                    .direct_link
                    .as_ref()
                    .and_then(|s| reqwest::Url::parse(s).ok())
                    .and_then(|u| riverdeck_core::marketplace::extract_download_url(&u))
                    .or_else(|| {
                        links
                            .deep_link
                            .as_ref()
                            .and_then(|s| reqwest::Url::parse(s).ok())
                            .and_then(|u| riverdeck_core::marketplace::extract_download_url(&u))
                    });
                if let Some(download_url) = candidate {
                    log::info!(
                        "Installing plugin via Elgato marketplace token: id={raw_id}, url={download_url}"
                    );
                    let res = riverdeck_core::api::plugins::install_plugin(
                        Some(download_url),
                        None,
                        Some(raw_id.to_owned()),
                    )
                    .await;
                    if let Err(err) = res {
                        log::warn!("Marketplace install failed (variant_id={raw_id}): {err:#}");
                        riverdeck_core::ui::emit(riverdeck_core::ui::UiEvent::PluginInstall {
                            id: raw_id.to_owned(),
                            phase: riverdeck_core::ui::PluginInstallPhase::Finished {
                                ok: false,
                                error: Some(marketplace_user_error(&err)),
                            },
                        });
                        return Ok(());
                    }
                    riverdeck_core::plugins::initialise_plugins();
                    return Ok(());
                }

                // Heuristic extraction failed; many marketplaces serve a signed download endpoint
                // without a helpful file extension in the URL. Fall back to trying the direct link.
                let redact = |s: &str| {
                    // Keep just scheme+host+path (drop query tokens).
                    reqwest::Url::parse(s)
                        .ok()
                        .map(|u| {
                            format!(
                                "{}://{}{}",
                                u.scheme(),
                                u.host_str().unwrap_or(""),
                                u.path()
                            )
                        })
                        .unwrap_or_else(|| "<unparseable>".to_owned())
                };
                log::warn!(
                    "Marketplace token available, but no archive URL extracted (variant_id={raw_id}, direct_link={}, deep_link={})",
                    links
                        .direct_link
                        .as_deref()
                        .map(redact)
                        .unwrap_or_else(|| "<none>".to_owned()),
                    links
                        .deep_link
                        .as_deref()
                        .map(redact)
                        .unwrap_or_else(|| "<none>".to_owned()),
                );

                if let Some(direct) = links.direct_link.as_ref()
                    && direct.trim().starts_with("https://")
                {
                    log::info!(
                        "Trying marketplace direct_link as install URL (variant_id={raw_id})"
                    );
                    let res = riverdeck_core::api::plugins::install_plugin(
                        Some(direct.trim().to_owned()),
                        None,
                        Some(raw_id.to_owned()),
                    )
                    .await;
                    if let Err(err) = res {
                        log::warn!(
                            "Marketplace direct_link install failed (variant_id={raw_id}): {err:#}"
                        );
                        riverdeck_core::ui::emit(riverdeck_core::ui::UiEvent::PluginInstall {
                            id: raw_id.to_owned(),
                            phase: riverdeck_core::ui::PluginInstallPhase::Finished {
                                ok: false,
                                error: Some(marketplace_user_error(&err)),
                            },
                        });
                        return Ok(());
                    }
                    riverdeck_core::plugins::initialise_plugins();
                    return Ok(());
                }
            }

            let item = riverdeck_core::marketplace::lookup_item_lite(raw_id).await;
            if let Some(item) = item.as_ref()
                && let Some(slug) = item.slug.as_ref()
                && !slug.trim().is_empty()
            {
                let product_url = format!("https://marketplace.elgato.com/product/{slug}");
                open_url_in_browser(&product_url);
            }

            let label = item
                .as_ref()
                .and_then(|i| i.product_name.as_deref())
                .or_else(|| item.as_ref().and_then(|i| i.name.as_deref()))
                .unwrap_or(raw_id)
                .to_owned();

            // UX: surface a toast so this doesn't feel like a no-op.
            riverdeck_core::ui::emit(riverdeck_core::ui::UiEvent::PluginInstall {
                id: label,
                phase: riverdeck_core::ui::PluginInstallPhase::Finished {
                    ok: false,
                    error: Some(
                        "Elgato Marketplace install links require a Marketplace session token to resolve into a download URL. Open the Marketplace window in RiverDeck (so it can capture your session), then try again — or download the .streamDeckPlugin and install via Settings → Plugins → Install…"
                            .to_owned(),
                    ),
                },
            });
            return Ok(());
        }

        if !plugin_id.is_empty() {
            log::warn!(
                "Received marketplace deep link for {plugin_id}, but couldn't extract a download URL (url={arg}); falling back to didReceiveDeepLink (if/when plugin is installed)"
            );

            // UX: surface a toast so this doesn't feel like a no-op.
            riverdeck_core::ui::emit(riverdeck_core::ui::UiEvent::PluginInstall {
                id: plugin_id
                    .strip_suffix(".sdPlugin")
                    .unwrap_or(plugin_id.as_str())
                    .to_owned(),
                phase: riverdeck_core::ui::PluginInstallPhase::Finished {
                    ok: false,
                    error: Some(
                        "Marketplace install link didn't include a direct download URL. Try downloading the plugin file and install it via Settings → Plugins → Install…"
                            .to_owned(),
                    ),
                },
            });

            let _ = riverdeck_core::events::outbound::deep_link::did_receive_deep_link(
                &plugin_id,
                arg.to_owned(),
            )
            .await;
        } else {
            log::warn!(
                "Received marketplace deep link, but couldn't extract a download URL or plugin id (url={arg}); ignoring"
            );
        }
        return Ok(());
    }

    // Generic icon pack deep-link handling (best-effort).
    if let Some(download_url) = extract_plugin_download_url(&url) {
        let download_l = download_url.to_lowercase();
        let is_icon_pack =
            download_l.contains(".streamdeckiconpack") || download_l.contains("streamdeckiconpack");
        if is_icon_pack {
            let fallback = if url.scheme() == "streamdeck" && !raw_id.is_empty() {
                raw_id.to_owned()
            } else {
                String::new()
            };
            log::info!("Installing icon pack via deep link: id={fallback}, url={download_url}");
            let _installed_id = riverdeck_core::api::icon_packs::install_icon_pack(
                download_url,
                if fallback.is_empty() {
                    None
                } else {
                    Some(fallback)
                },
            )
            .await?;
        }
    }

    Ok(())
}

fn extract_plugin_download_url(url: &reqwest::Url) -> Option<String> {
    riverdeck_core::marketplace::extract_download_url(url)
}

fn marketplace_user_error(err: &anyhow::Error) -> String {
    let s = format!("{err:#}");
    if s.contains("Elgato-protected binary format")
        || s.contains("ELGATO header")
        || s.contains("manifest is in Elgato-protected")
    {
        return "This Marketplace plugin uses Elgato's protected manifest format (\"ELGATO\" manifest container), which RiverDeck can't import yet."
            .to_owned();
    }
    s
}

fn start_core_background(runtime: Arc<Runtime>) {
    runtime.spawn(async {
        loop {
            elgato::initialise_devices().await;
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        }
    });

    runtime.spawn(async {
        // Dev convenience: if we aren't running from a packaged "resources/plugins" bundle,
        // build the repo's built-in plugins into the user's config dir so the app doesn't
        // silently keep using stale plugins from older installs.
        maybe_build_repo_builtin_plugins().await;
        plugins::initialise_plugins();
        application_watcher::init_application_watcher();
    });
}

fn host_target_triple() -> String {
    if let Ok(t) = std::env::var("RIVERDECK_PLUGIN_BUILD_TARGET") {
        let t = t.trim().to_owned();
        if !t.is_empty() {
            return t;
        }
    }

    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    match (os, arch) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu".to_owned(),
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu".to_owned(),
        ("macos", "aarch64") => "aarch64-apple-darwin".to_owned(),
        ("macos", "x86_64") => "x86_64-apple-darwin".to_owned(),
        ("windows", "x86_64") => "x86_64-pc-windows-msvc".to_owned(),
        // Best-effort fallback; users can override via `RIVERDECK_PLUGIN_BUILD_TARGET`.
        _ => format!("{arch}-{os}"),
    }
}

async fn maybe_build_repo_builtin_plugins() {
    // Only do this in debug builds (or when explicitly requested), since it can be slow.
    let enabled = cfg!(debug_assertions) || env_truthy_any("RIVERDECK_BUILD_BUILTIN_PLUGINS");
    if !enabled {
        return;
    }

    // If we're running from a packaged bundle (resources/plugins), use that instead.
    if shared::resource_dir().is_some() {
        return;
    }

    let Ok(repo_root) = std::env::current_dir() else {
        return;
    };
    let repo_plugins_dir = repo_root.join("plugins");
    if !repo_plugins_dir.is_dir() {
        return;
    }

    // Only attempt if `deno` is available (plugin build scripts are Deno-based).
    let deno_ok = tokio::process::Command::new("deno")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);
    if !deno_ok {
        log::debug!(
            "Skipping repo builtin plugin build (missing `deno` on PATH). Set RIVERDECK_BUILD_BUILTIN_PLUGINS=0 to silence."
        );
        return;
    }

    let target = host_target_triple();
    let config_plugins_dir = shared::config_dir().join("plugins");
    let _ = tokio::fs::create_dir_all(&config_plugins_dir).await;
    let temp_root = shared::config_dir()
        .join("temp")
        .join("repo_builtin_plugins");
    let _ = tokio::fs::create_dir_all(&temp_root).await;

    let mut entries = match tokio::fs::read_dir(&repo_plugins_dir).await {
        Ok(e) => e,
        Err(_) => return,
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let plugin_src = entry.path();
        let meta = match entry.metadata().await {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_dir() {
            continue;
        }

        // Repo plugin source layout:
        // - `plugins/<id>.sdPlugin/build.ts`
        // - `plugins/<id>.sdPlugin/assets/manifest.json`
        let build_ts = plugin_src.join("build.ts");
        let assets_manifest = plugin_src.join("assets").join("manifest.json");
        if !build_ts.is_file() || !assets_manifest.is_file() {
            continue;
        }

        let Some(id) = plugin_src
            .file_name()
            .and_then(|x| x.to_str())
            .map(|s| s.to_owned())
        else {
            continue;
        };

        // Compare versions (installed vs source) to avoid rebuilding every launch.
        let installed_dir = config_plugins_dir.join(&id);
        let src_ver = tokio::fs::read(&assets_manifest)
            .await
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| {
                v.get("Version")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_owned())
            });
        let installed_ver = tokio::fs::read(installed_dir.join("manifest.json"))
            .await
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| {
                v.get("Version")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_owned())
            });
        if src_ver.is_some()
            && installed_ver.is_some()
            && src_ver.as_deref() == installed_ver.as_deref()
        {
            continue;
        }

        let out_dir = temp_root.join(format!("{}_{}", id.replace('/', "_"), std::process::id()));
        let _ = tokio::fs::remove_dir_all(&out_dir).await;

        log::info!("Building builtin plugin from repo: {id} (target={target})");
        let status = tokio::process::Command::new("deno")
            .arg("run")
            .arg("-A")
            .arg(&build_ts)
            .arg(&out_dir)
            .arg(&target)
            .status()
            .await;

        let Ok(status) = status else {
            log::warn!("Failed to spawn deno to build {id}");
            continue;
        };
        if !status.success() {
            log::warn!("Failed to build repo builtin plugin {id} (deno exit={status})");
            continue;
        }

        // Swap into place atomically (best-effort). If something goes wrong, keep the previous plugin.
        let backup = installed_dir.with_extension("old");
        if tokio::fs::try_exists(&installed_dir).await.unwrap_or(false) {
            let _ = tokio::fs::remove_dir_all(&backup).await;
            let _ = tokio::fs::rename(&installed_dir, &backup).await;
        }
        if tokio::fs::rename(&out_dir, &installed_dir).await.is_err() {
            // Roll back if we can.
            let _ = tokio::fs::remove_dir_all(&installed_dir).await;
            if tokio::fs::try_exists(&backup).await.unwrap_or(false) {
                let _ = tokio::fs::rename(&backup, &installed_dir).await;
            }
        } else {
            let _ = tokio::fs::remove_dir_all(&backup).await;
        }

        // If we just installed the new RiverDeck starter pack, hide any legacy OpenDeck plugin folder.
        if id == riverdeck_core::shared::STARTERPACK_PLUGIN_ID {
            let legacy =
                config_plugins_dir.join(riverdeck_core::shared::LEGACY_STARTERPACK_PLUGIN_ID);
            if tokio::fs::try_exists(&legacy).await.unwrap_or(false) {
                let legacy_old = legacy.with_extension("old");
                let _ = tokio::fs::remove_dir_all(&legacy_old).await;
                let _ = tokio::fs::rename(&legacy, &legacy_old).await;
            }
        }
    }
}

struct RiverDeckApp {
    runtime: Arc<Runtime>,
    ui_events: Option<tokio_mpsc::UnboundedReceiver<ui::UiEvent>>,

    // Keep the lock file alive for the lifetime of the app.
    #[allow(dead_code)]
    _lock_file: std::fs::File,

    start_hidden: bool,
    update_info: Arc<Mutex<Option<UpdateInfo>>>,

    #[cfg(feature = "tray")]
    tray: Option<TrayState>,
    #[cfg(feature = "tray")]
    tray_hide_ok: bool,
    #[cfg(feature = "tray")]
    hide_to_tray_requested: bool,
    selected_device: Option<String>,

    selected_slot: Option<SelectedSlot>,
    // Per-button label editor (host-side, persisted in ActionState.text + ActionState.text_placement).
    button_label_context: Option<shared::ActionContext>,
    button_label_input: String,
    button_label_placement: shared::TextPlacement,
    button_show_title: bool,
    button_show_action_name: bool,
    // Native action options editor (replaces PI for supported actions).
    options_editor_context: Option<shared::ActionContext>,
    // Schema-driven action options editor (host registry; covers PI-based actions).
    schema_editor_context: Option<shared::ActionContext>,
    schema_editor_draft: serde_json::Value,
    schema_editor_error: Option<String>,
    // Starterpack: Run Command
    sp_run_command_down: String,
    sp_run_command_up: String,
    sp_run_command_rotate: String,
    sp_run_command_file: String,
    sp_run_command_show: bool,
    // Starterpack: Input Simulation
    sp_input_sim_down: String,
    sp_input_sim_up: String,
    sp_input_sim_anticlockwise: String,
    sp_input_sim_clockwise: String,
    // Starterpack: Switch Profile
    sp_switch_profile_device: String,
    sp_switch_profile_profile: String,
    // Starterpack: Device Brightness
    sp_device_brightness_action: String, // "set" | "increase" | "decrease"
    sp_device_brightness_value: i32,     // 0..=100
    action_search: String,
    texture_cache: HashMap<String, CachedTexture>,

    // Cached async data to avoid blocking the UI thread during paint.
    categories_cache: Option<Vec<(String, shared::Category)>>,
    categories_inflight: bool,
    categories_rx: Option<mpsc::Receiver<std::collections::HashMap<String, shared::Category>>>,

    plugins_cache: Option<Vec<riverdeck_core::plugins::marketplace::LocalPluginEntry>>,
    plugins_inflight: bool,
    plugins_rx: Option<mpsc::Receiver<Vec<riverdeck_core::plugins::marketplace::LocalPluginEntry>>>,

    pages_cache: HashMap<(String, String), Vec<String>>, // (device_id, profile_id) -> pages
    pages_inflight: Option<(String, String)>,
    pages_rx: Option<PagesRx>,

    action_controller_filter: String,
    drag_payload: Option<DragPayload>,
    drag_hover_slot: Option<SelectedSlot>,
    drag_hover_valid: bool,

    // Bottom action editor panel UX (collapsed tab + expandable sheet).
    #[allow(dead_code)]
    action_editor_open: bool,

    show_update_details: bool,
    show_settings: bool,
    settings_autostart: bool,
    settings_screensaver: bool,
    settings_error: Option<String>,

    // Marketplace window state (a simple webview window via `riverdeck-pi`).
    marketplace_child: Option<std::process::Child>,
    marketplace_last_error: Option<String>,

    // Property Inspector window state (a simple webview window via `riverdeck-pi`).
    property_inspector_child: Option<std::process::Child>,
    property_inspector_context: Option<shared::ActionContext>,
    property_inspector_last_error: Option<String>,

    // Profile management UI.
    show_profile_editor: bool,
    profile_name_input: String,
    profile_error: Option<String>,

    // Plugin management UI.
    show_manage_plugins: bool,
    plugin_manage_error: Option<String>,
    manage_plugins_show_action_uuids: bool,
    #[allow(dead_code)]
    pending_plugin_install_pick: Option<mpsc::Receiver<Option<PathBuf>>>,
    pending_plugin_install_result: Option<mpsc::Receiver<Result<(), String>>>,
    pending_plugin_remove_confirm: Option<(String, String)>, // (id, name)
    pending_plugin_remove_result: Option<mpsc::Receiver<Result<(), String>>>,

    // Toast notifications (bottom-right).
    toasts: ToastManager,

    // Non-blocking file picking (run dialogs off the UI thread).
    pending_icon_pick: Option<(mpsc::Receiver<Option<PathBuf>>, shared::ActionContext, u16)>,
    pending_screen_bg_pick: Option<(mpsc::Receiver<Option<PathBuf>>, String, String)>, // (rx, device_id, profile_id)

    // Icon Library (built-in + marketplace icon packs).
    icon_library_open: Option<(shared::ActionContext, u16)>,
    icon_library_packs_cache: Vec<riverdeck_core::api::icon_packs::IconPackInfo>,
    icon_library_selected_pack: Option<String>,
    icon_library_query: String,
    icon_library_icons_cache: Vec<riverdeck_core::api::icon_packs::IconInfo>,
    icon_library_cache_key: Option<(String, String)>, // (pack_id, query)
    icon_library_error: Option<String>,

    // Cached center of the main preview area so overlays can align with it (not with side panels).
    preview_center_x: Option<f32>,
    // Cached width of the main preview area so overlays scale like it (not like side panels).
    preview_width: Option<f32>,
}

#[derive(Debug, Clone)]
enum ToastLevel {
    Info,
    Success,
    Error,
}

#[derive(Debug, Clone)]
struct LocalMarketplacePluginRow {
    id: String,
    name: String,
    icon: Option<String>,
    version: Option<String>,
    installed: bool,
    enabled: bool,
    source: riverdeck_core::plugins::marketplace::PluginSource,
    support: riverdeck_core::plugins::marketplace::PluginSupport,
    path: String,
}

#[derive(Debug, Clone)]
struct Toast {
    title: String,
    body: Option<String>,
    level: ToastLevel,
    created_at: std::time::Instant,
    expires_at: Option<std::time::Instant>,
}

#[derive(Debug, Default)]
struct ToastManager {
    // Toasts keyed by install id for "sticky until done" installs.
    install_toasts: HashMap<String, Toast>,
    // Completed toasts (success/failure) with expiry.
    completed: Vec<Toast>,
}

impl ToastManager {
    fn is_active(&self) -> bool {
        !self.install_toasts.is_empty() || !self.completed.is_empty()
    }

    fn gc(&mut self) {
        let now = std::time::Instant::now();
        self.completed
            .retain(|t| t.expires_at.is_none_or(|e| e > now));
    }

    fn set_installing(&mut self, id: &str) {
        let now = std::time::Instant::now();
        let toast = Toast {
            title: format!("Installing {id}…"),
            body: None,
            level: ToastLevel::Info,
            created_at: now,
            expires_at: None,
        };
        self.install_toasts.insert(id.to_owned(), toast);
    }

    fn finish_install(&mut self, id: &str, ok: bool, error: Option<String>) {
        let now = std::time::Instant::now();
        self.install_toasts.remove(id);

        let (title, body, level, ttl) = if ok {
            (
                format!("Installed {id}"),
                None,
                ToastLevel::Success,
                std::time::Duration::from_secs(5),
            )
        } else {
            (
                format!("Failed to install {id}"),
                error,
                ToastLevel::Error,
                std::time::Duration::from_secs(10),
            )
        };

        self.completed.push(Toast {
            title,
            body,
            level,
            created_at: now,
            expires_at: Some(now + ttl),
        });
    }
}

type PagesKey = (String, String);
type PagesRxPayload = (PagesKey, Vec<String>);
type PagesRx = mpsc::Receiver<PagesRxPayload>;

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectedSlot {
    controller: String,
    position: u8,
}

#[derive(Clone)]
enum DragPayload {
    Action(shared::Action),
    Slot {
        device_id: String,
        profile_id: String,
        page_id: String,
        slot: SelectedSlot,
        preview_name: String,
        preview_image_path: Option<String>,
    },
}

struct CachedTexture {
    modified: Option<std::time::SystemTime>,
    texture: egui::TextureHandle,
}

#[derive(Clone)]
struct ProfileSnapshot {
    page_id: String,
    keys: Vec<Option<shared::ActionInstance>>,
    sliders: Vec<Option<shared::ActionInstance>>,
    encoder_screen_background: Option<String>,
}

impl RiverDeckApp {
    const DEVICES_PANEL_DEFAULT_WIDTH: f32 = 200.0; // egui::SidePanel default in egui 0.31.x
    const ACTIONS_PANEL_DEFAULT_WIDTH: f32 = 280.0; // allow Actions panel to be half the previous width
    const POPUP_TITLEBAR_HEIGHT: f32 = 28.0;

    fn draw_popup_titlebar(ui: &mut egui::Ui, title: &str) -> bool {
        let height = Self::POPUP_TITLEBAR_HEIGHT;
        let (_size, resp) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), height),
            egui::Sense::click_and_drag(),
        );
        let rect = resp.rect;
        let mut close_requested = false;

        // Match the "top" area look of the main window: it shows the viewport background
        // (window_fill), since the top panel is `Frame::NONE`.
        let fill = ui.visuals().window_fill;

        let cr = ui.visuals().window_corner_radius;
        let rounding = egui::CornerRadius {
            nw: cr.nw,
            ne: cr.ne,
            sw: 0,
            se: 0,
        };
        ui.painter().rect_filled(rect, rounding, fill);

        // Split into two regions so the close button is always flush-right.
        let outer_pad = 6.0;
        let buttons_w = 44.0;
        let right_x1 = rect.right() - outer_pad;
        let right_x0 = (right_x1 - buttons_w).max(rect.left());
        let right_rect = egui::Rect::from_min_max(
            egui::pos2(right_x0, rect.top()),
            egui::pos2(right_x1, rect.bottom()),
        );
        let left_rect = egui::Rect::from_min_max(
            rect.left_top(),
            egui::pos2(right_rect.left(), rect.bottom()),
        );

        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(left_rect), |ui| {
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add_space(10.0);
                ui.label(egui::RichText::new(title).strong());
            });
        });

        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(right_rect), |ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                ui.spacing_mut().button_padding = egui::vec2(7.0, 3.0);
                let btn_size = egui::vec2(26.0, 20.0);
                if ui.add(egui::Button::new("X").min_size(btn_size)).clicked() {
                    close_requested = true;
                }
            });
        });

        // Divider between the header and content.
        ui.painter().hline(
            rect.x_range(),
            rect.bottom(),
            egui::Stroke::new(1.0, ui.visuals().widgets.inactive.bg_stroke.color),
        );

        close_requested
    }

    fn new(
        cc: &eframe::CreationContext<'_>,
        runtime: Arc<Runtime>,
        lock_file: std::fs::File,
        start_hidden: bool,
        update_info: Arc<Mutex<Option<UpdateInfo>>>,
    ) -> Self {
        log::info!("RiverDeckApp::new(start_hidden={start_hidden})");
        #[cfg(feature = "tray")]
        let tray = match TrayState::new() {
            Ok(t) => Some(t),
            Err(err) => {
                log::warn!("Tray icon disabled (failed to initialize): {err:#}");
                // Also write to a file for Wayland/desktop-entry launches where logs aren't visible.
                log_tray_status_to_file(&format!("tray init failed: {err:#}"));
                None
            }
        };
        #[cfg(feature = "tray")]
        if tray.is_some() {
            log_tray_status_to_file("tray init ok");
        }
        #[cfg(feature = "tray")]
        let tray_hide_ok = tray.is_some() && tray_hide_is_safe_by_default();
        #[cfg(feature = "tray")]
        if tray.is_some() && !tray_hide_ok {
            log::warn!(
                "Tray icon initialized but hide-to-tray is disabled for this desktop (set RIVERDECK_FORCE_TRAY=1 to override)"
            );
            log_tray_status_to_file("tray init ok, but hide-to-tray disabled by desktop heuristic");
        }

        let ui_events = ui::subscribe().map(|mut rx| {
            let (tx, out_rx) = tokio_mpsc::unbounded_channel::<ui::UiEvent>();
            let egui_ctx = cc.egui_ctx.clone();
            runtime.spawn(async move {
                let mut last_repaint = std::time::Instant::now()
                    .checked_sub(std::time::Duration::from_secs(1))
                    .unwrap_or_else(std::time::Instant::now);
                loop {
                    match rx.recv().await {
                        Ok(ev) => {
                            let _ = tx.send(ev);
                            // Throttle wakeups: UI events can be frequent; we only need to ensure
                            // we get *a* frame soon after background state changes (e.g. toasts).
                            let now = std::time::Instant::now();
                            if now.duration_since(last_repaint)
                                >= std::time::Duration::from_millis(50)
                            {
                                egui_ctx.request_repaint();
                                last_repaint = now;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
            out_rx
        });

        Self {
            runtime,
            ui_events,
            _lock_file: lock_file,
            start_hidden,
            update_info,
            #[cfg(feature = "tray")]
            tray,
            #[cfg(feature = "tray")]
            tray_hide_ok,
            #[cfg(feature = "tray")]
            hide_to_tray_requested: false,
            selected_device: None,
            selected_slot: None,
            button_label_context: None,
            button_label_input: String::new(),
            button_label_placement: shared::TextPlacement::Bottom,
            button_show_title: true,
            button_show_action_name: true,
            options_editor_context: None,
            schema_editor_context: None,
            schema_editor_draft: serde_json::Value::Object(serde_json::Map::new()),
            schema_editor_error: None,
            sp_run_command_down: String::new(),
            sp_run_command_up: String::new(),
            sp_run_command_rotate: String::new(),
            sp_run_command_file: String::new(),
            sp_run_command_show: false,
            sp_input_sim_down: String::new(),
            sp_input_sim_up: String::new(),
            sp_input_sim_anticlockwise: String::new(),
            sp_input_sim_clockwise: String::new(),
            sp_switch_profile_device: String::new(),
            sp_switch_profile_profile: "Default".to_owned(),
            sp_device_brightness_action: "set".to_owned(),
            sp_device_brightness_value: 50,
            action_search: String::new(),
            texture_cache: HashMap::new(),
            categories_cache: None,
            categories_inflight: false,
            categories_rx: None,
            plugins_cache: None,
            plugins_inflight: false,
            plugins_rx: None,
            pages_cache: HashMap::new(),
            pages_inflight: None,
            pages_rx: None,
            action_controller_filter: "Keypad".to_owned(),
            drag_payload: None,
            drag_hover_slot: None,
            drag_hover_valid: false,
            action_editor_open: false,
            show_update_details: false,
            show_settings: false,
            settings_autostart: false,
            settings_screensaver: false,
            settings_error: None,
            marketplace_child: None,
            marketplace_last_error: None,
            property_inspector_child: None,
            property_inspector_context: None,
            property_inspector_last_error: None,
            show_profile_editor: false,
            profile_name_input: String::new(),
            profile_error: None,
            show_manage_plugins: false,
            plugin_manage_error: None,
            manage_plugins_show_action_uuids: false,
            pending_plugin_install_pick: None,
            pending_plugin_install_result: None,
            pending_plugin_remove_confirm: None,
            pending_plugin_remove_result: None,
            toasts: ToastManager::default(),
            pending_icon_pick: None,
            pending_screen_bg_pick: None,
            icon_library_open: None,
            icon_library_packs_cache: vec![],
            icon_library_selected_pack: None,
            icon_library_query: String::new(),
            icon_library_icons_cache: vec![],
            icon_library_cache_key: None,
            icon_library_error: None,
            preview_center_x: None,
            preview_width: None,
        }
    }

    #[cfg(feature = "tray")]
    fn hide_to_tray_available(&self) -> bool {
        self.tray.is_some() && self.tray_hide_ok
    }

    fn native_options_kind(action_uuid: &str) -> Option<&'static str> {
        match action_uuid {
            "io.github.sulrwin.riverdeck.starterpack.runcommand" => Some("starterpack_runcommand"),
            "io.github.sulrwin.riverdeck.starterpack.inputsimulation" => {
                Some("starterpack_inputsimulation")
            }
            "io.github.sulrwin.riverdeck.starterpack.switchprofile" => {
                Some("starterpack_switchprofile")
            }
            "io.github.sulrwin.riverdeck.starterpack.devicebrightness" => {
                Some("starterpack_devicebrightness")
            }
            _ => None,
        }
    }

    fn ensure_options_editor_state(
        &mut self,
        device: &shared::DeviceInfo,
        instance: &shared::ActionInstance,
    ) {
        let needs_reset = match self.options_editor_context.as_ref() {
            None => true,
            Some(c) => c != &instance.context,
        };
        if !needs_reset {
            return;
        }

        self.options_editor_context = Some(instance.context.clone());

        let s = &instance.settings;
        let get_s =
            |k: &str| -> String { s.get(k).and_then(|v| v.as_str()).unwrap_or("").to_owned() };
        let get_bool = |k: &str, default: bool| -> bool {
            s.get(k).and_then(|v| v.as_bool()).unwrap_or(default)
        };
        let get_i = |k: &str, default: i32| -> i32 {
            s.get(k)
                .and_then(|v| v.as_i64())
                .map(|v| v as i32)
                .unwrap_or(default)
        };

        match Self::native_options_kind(instance.action.uuid.as_str()) {
            Some("starterpack_runcommand") => {
                self.sp_run_command_down = get_s("down");
                self.sp_run_command_up = get_s("up");
                self.sp_run_command_rotate = get_s("rotate");
                self.sp_run_command_file = get_s("file");
                self.sp_run_command_show = get_bool("show", false);
            }
            Some("starterpack_inputsimulation") => {
                self.sp_input_sim_down = get_s("down");
                self.sp_input_sim_up = get_s("up");
                self.sp_input_sim_anticlockwise = get_s("anticlockwise");
                self.sp_input_sim_clockwise = get_s("clockwise");
            }
            Some("starterpack_switchprofile") => {
                self.sp_switch_profile_device = s
                    .get("device")
                    .and_then(|v| v.as_str())
                    .unwrap_or(device.id.as_str())
                    .to_owned();
                self.sp_switch_profile_profile = s
                    .get("profile")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Default")
                    .to_owned();
            }
            Some("starterpack_devicebrightness") => {
                self.sp_device_brightness_action = s
                    .get("action")
                    .and_then(|v| v.as_str())
                    .unwrap_or("set")
                    .to_owned();
                self.sp_device_brightness_value = get_i("value", 50).clamp(0, 100);
            }
            _ => {}
        }
    }

    fn spawn_set_instance_settings(&self, ctx: shared::ActionContext, settings: serde_json::Value) {
        self.runtime.spawn(async move {
            let _ = riverdeck_core::api::instances::set_instance_settings(ctx, settings).await;
        });
    }

    fn ensure_schema_editor_state(&mut self, instance: &shared::ActionInstance) {
        let needs_reset = match self.schema_editor_context.as_ref() {
            None => true,
            Some(c) => c != &instance.context,
        };
        if !needs_reset {
            return;
        }
        self.schema_editor_context = Some(instance.context.clone());
        self.schema_editor_draft = instance.settings.clone();
        self.schema_editor_error = None;
    }

    fn draw_action_editor_schema_right_column(
        &mut self,
        ui: &mut egui::Ui,
        instance: &shared::ActionInstance,
    ) {
        use riverdeck_core::options_schema as os;

        let Some(schema) = os::get_schema(instance.action.uuid.as_str()) else {
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("No schema for this action.")
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
            return;
        };

        self.ensure_schema_editor_state(instance);

        ui.add_space(6.0);
        ui.label(egui::RichText::new("Options").strong());
        ui.label(
            egui::RichText::new(schema.title)
                .small()
                .color(ui.visuals().weak_text_color()),
        );

        if let Some(err) = self.schema_editor_error.as_ref() {
            ui.colored_label(ui.visuals().error_fg_color, err);
        }

        ui.add_space(6.0);

        let mut changed_any = false;
        for field in schema.fields {
            ui.push_id(field.key_path, |ui| {
                ui.label(field.label);
                if let Some(help) = field.help {
                    ui.label(
                        egui::RichText::new(help)
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    );
                }

                let current = os::get_by_path(&self.schema_editor_draft, field.key_path)
                    .cloned()
                    .unwrap_or_else(|| field.default.to_json());

                match &field.kind {
                    os::FieldKind::Bool => {
                        let mut v = current.as_bool().unwrap_or(false);
                        let r = ui.checkbox(&mut v, "");
                        if r.changed() {
                            changed_any = true;
                            if let Err(e) = os::set_by_path(
                                &mut self.schema_editor_draft,
                                field.key_path,
                                serde_json::Value::Bool(v),
                            ) {
                                self.schema_editor_error = Some(e.to_owned());
                            } else {
                                self.schema_editor_error = None;
                            }
                        }
                    }
                    os::FieldKind::Int => {
                        let mut v: i64 = current.as_i64().unwrap_or(match field.default {
                            os::DefaultValue::Int(i) => i,
                            _ => 0,
                        });
                        let min_v = field.min.map(|m| m as i64).unwrap_or(i64::MIN);
                        let max_v = field.max.map(|m| m as i64).unwrap_or(i64::MAX);
                        let dv = egui::DragValue::new(&mut v).range(min_v..=max_v);
                        let r = ui.add(dv);
                        if r.changed() {
                            changed_any = true;
                            if let Err(e) = os::set_by_path(
                                &mut self.schema_editor_draft,
                                field.key_path,
                                serde_json::Value::Number(v.into()),
                            ) {
                                self.schema_editor_error = Some(e.to_owned());
                            } else {
                                self.schema_editor_error = None;
                            }
                        }
                    }
                    os::FieldKind::Float => {
                        let mut v: f64 = current.as_f64().unwrap_or(match field.default {
                            os::DefaultValue::Float(f) => f,
                            _ => 0.0,
                        });
                        let min_v = field.min.unwrap_or(f64::MIN);
                        let max_v = field.max.unwrap_or(f64::MAX);
                        let dv = egui::DragValue::new(&mut v).range(min_v..=max_v);
                        let r = ui.add(dv);
                        if r.changed() {
                            changed_any = true;
                            let num = serde_json::Number::from_f64(v)
                                .map(serde_json::Value::Number)
                                .unwrap_or(serde_json::Value::Null);
                            if let Err(e) =
                                os::set_by_path(&mut self.schema_editor_draft, field.key_path, num)
                            {
                                self.schema_editor_error = Some(e.to_owned());
                            } else {
                                self.schema_editor_error = None;
                            }
                        }
                    }
                    os::FieldKind::Text | os::FieldKind::PathFile | os::FieldKind::PathDir => {
                        let mut v = current.as_str().unwrap_or("").to_owned();
                        let r = ui.add_sized(
                            [ui.available_width(), 0.0],
                            egui::TextEdit::singleline(&mut v).font(egui::TextStyle::Monospace),
                        );
                        if r.changed() {
                            changed_any = true;
                            if let Err(e) = os::set_by_path(
                                &mut self.schema_editor_draft,
                                field.key_path,
                                serde_json::Value::String(v),
                            ) {
                                self.schema_editor_error = Some(e.to_owned());
                            } else {
                                self.schema_editor_error = None;
                            }
                        }
                    }
                    os::FieldKind::Enum(opts) => {
                        let mut selected = current.as_str().unwrap_or("").to_owned();
                        let before = selected.clone();
                        let mut displayed = selected.clone();
                        if displayed.is_empty() {
                            displayed = match field.default {
                                os::DefaultValue::Text(t) => t.to_owned(),
                                _ => "<none>".to_owned(),
                            };
                        }
                        egui::ComboBox::from_id_salt("enum")
                            .width(ui.available_width().min(240.0))
                            .selected_text(displayed)
                            .show_ui(ui, |ui| {
                                for o in opts.iter() {
                                    ui.selectable_value(&mut selected, o.value.to_owned(), o.label);
                                }
                            });
                        if selected != before {
                            changed_any = true;
                            if let Err(e) = os::set_by_path(
                                &mut self.schema_editor_draft,
                                field.key_path,
                                serde_json::Value::String(selected),
                            ) {
                                self.schema_editor_error = Some(e.to_owned());
                            } else {
                                self.schema_editor_error = None;
                            }
                        }
                    }
                }
                ui.add_space(6.0);
            });
        }

        ui.separator();
        ui.horizontal(|ui| {
            let reset_clicked = ui.button("Reset").clicked();
            if reset_clicked {
                self.schema_editor_draft = instance.settings.clone();
                self.schema_editor_error = None;
            }

            let is_dirty = self.schema_editor_draft != instance.settings;
            let apply = ui.add_enabled(is_dirty, egui::Button::new("Apply"));
            if apply.clicked() {
                let ctx_to_set = instance.context.clone();
                let settings = self.schema_editor_draft.clone();
                self.spawn_set_instance_settings(ctx_to_set, settings);
            } else if changed_any {
                // no-op: kept for clarity; we want `changed_any` to influence `is_dirty` only.
            }
        });
    }

    fn draw_action_editor_native_options_right_column(
        &mut self,
        ui: &mut egui::Ui,
        device: &shared::DeviceInfo,
        instance: &shared::ActionInstance,
    ) {
        ui.add_space(6.0);
        ui.label(egui::RichText::new("Options").strong());

        match Self::native_options_kind(instance.action.uuid.as_str()) {
            Some("starterpack_runcommand") => {
                let is_encoder = instance.context.controller == "Encoder";
                let down_label = if is_encoder { "Dial down" } else { "Key down" };
                let up_label = if is_encoder { "Dial up" } else { "Key up" };

                ui.add_space(4.0);
                ui.label(down_label);
                let changed_down = ui
                    .add_sized(
                        [ui.available_width(), 0.0],
                        egui::TextEdit::singleline(&mut self.sp_run_command_down)
                            .font(egui::TextStyle::Monospace),
                    )
                    .changed();

                ui.label(up_label);
                let changed_up = ui
                    .add_sized(
                        [ui.available_width(), 0.0],
                        egui::TextEdit::singleline(&mut self.sp_run_command_up)
                            .font(egui::TextStyle::Monospace),
                    )
                    .changed();

                let mut changed_rotate = false;
                if is_encoder {
                    ui.label("Dial rotate");
                    changed_rotate = ui
                        .add_sized(
                            [ui.available_width(), 0.0],
                            egui::TextEdit::singleline(&mut self.sp_run_command_rotate)
                                .font(egui::TextStyle::Monospace),
                        )
                        .changed();
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(
                                "Tip: %d is replaced with the number of ticks (negative = CCW, positive = CW).",
                            )
                            .small()
                            .color(ui.visuals().weak_text_color()),
                        )
                        .wrap(),
                    );
                }

                ui.add_space(6.0);
                ui.label("Write to path");
                let changed_file = ui
                    .add_sized(
                        [ui.available_width(), 0.0],
                        egui::TextEdit::singleline(&mut self.sp_run_command_file)
                            .font(egui::TextStyle::Monospace),
                    )
                    .changed();

                let changed_show = ui
                    .checkbox(&mut self.sp_run_command_show, "Show on key")
                    .changed();

                if changed_down || changed_up || changed_rotate || changed_file || changed_show {
                    let ctx_to_set = instance.context.clone();
                    let settings = serde_json::json!({
                        "down": self.sp_run_command_down,
                        "up": self.sp_run_command_up,
                        "rotate": self.sp_run_command_rotate,
                        "file": self.sp_run_command_file,
                        "show": self.sp_run_command_show,
                    });
                    self.spawn_set_instance_settings(ctx_to_set, settings);
                }
            }
            Some("starterpack_inputsimulation") => {
                let is_encoder = instance.context.controller == "Encoder";
                let down_label = if is_encoder { "Dial down" } else { "Key down" };
                let up_label = if is_encoder { "Dial up" } else { "Key up" };

                ui.add_space(4.0);
                ui.label(down_label);
                let changed_down = ui
                    .add_sized(
                        [ui.available_width(), 0.0],
                        egui::TextEdit::singleline(&mut self.sp_input_sim_down)
                            .font(egui::TextStyle::Monospace)
                            .hint_text("Input to be executed on key/dial down"),
                    )
                    .changed();

                ui.label(up_label);
                let changed_up = ui
                    .add_sized(
                        [ui.available_width(), 0.0],
                        egui::TextEdit::singleline(&mut self.sp_input_sim_up)
                            .font(egui::TextStyle::Monospace)
                            .hint_text("Input to be executed on key/dial up"),
                    )
                    .changed();

                let mut changed_enc = false;
                if is_encoder {
                    ui.add_space(4.0);
                    ui.label("Dial rotate anticlockwise");
                    changed_enc |= ui
                        .add_sized(
                            [ui.available_width(), 0.0],
                            egui::TextEdit::singleline(&mut self.sp_input_sim_anticlockwise)
                                .font(egui::TextStyle::Monospace)
                                .hint_text("Input to be executed on dial rotate anticlockwise"),
                        )
                        .changed();
                    ui.label("Dial rotate clockwise");
                    changed_enc |= ui
                        .add_sized(
                            [ui.available_width(), 0.0],
                            egui::TextEdit::singleline(&mut self.sp_input_sim_clockwise)
                                .font(egui::TextStyle::Monospace)
                                .hint_text("Input to be executed on dial rotate clockwise"),
                        )
                        .changed();
                }

                if changed_down || changed_up || changed_enc {
                    let ctx_to_set = instance.context.clone();
                    let settings = serde_json::json!({
                        "down": self.sp_input_sim_down,
                        "up": self.sp_input_sim_up,
                        "anticlockwise": self.sp_input_sim_anticlockwise,
                        "clockwise": self.sp_input_sim_clockwise,
                    });
                    self.spawn_set_instance_settings(ctx_to_set, settings);
                }

                ui.add_space(6.0);
                egui::CollapsingHeader::new("Details")
                    .default_open(true)
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(
                                    "[t(\"hello world!\"),m(10,10,r),s(5),b(l),k(ctrl,p),k(uni('a')),k(ctrl,r)]",
                                )
                                .monospace(),
                            )
                            .wrap(),
                        );
                        ui.add_space(4.0);
                        ui.label("The example above becomes something like this:");
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(
                                    "[\n  Text(\"hello world!\"),\n  MoveMouse(10, 10, Relative),\n  Scroll(5, Vertical),\n  Button(Left, Click),\n  Key(Control, Press),\n  Key(Unicode('a'), Click),\n  Key(Control, Release),\n]",
                                )
                                .monospace(),
                            )
                            .wrap(),
                        );
                        ui.add_space(4.0);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(
                                    "Tip: keycodes/buttons/axes are defined by enigo; see their docs for the full list.",
                                )
                                .small()
                                .color(ui.visuals().weak_text_color()),
                            )
                            .wrap(),
                        );
                    });
            }
            Some("starterpack_switchprofile") => {
                ui.add_space(4.0);
                ui.label("Device ID");
                let changed_dev = ui
                    .add_sized(
                        [ui.available_width(), 0.0],
                        egui::TextEdit::singleline(&mut self.sp_switch_profile_device)
                            .hint_text(device.id.as_str()),
                    )
                    .changed();
                ui.label("Profile");
                let changed_prof = ui
                    .add_sized(
                        [ui.available_width(), 0.0],
                        egui::TextEdit::singleline(&mut self.sp_switch_profile_profile)
                            .hint_text("Default"),
                    )
                    .changed();

                if changed_dev || changed_prof {
                    let ctx_to_set = instance.context.clone();
                    let device_id = if self.sp_switch_profile_device.trim().is_empty() {
                        device.id.clone()
                    } else {
                        self.sp_switch_profile_device.clone()
                    };
                    let settings = serde_json::json!({
                        "device": device_id,
                        "profile": self.sp_switch_profile_profile,
                    });
                    self.spawn_set_instance_settings(ctx_to_set, settings);
                }
            }
            Some("starterpack_devicebrightness") => {
                if instance.context.controller == "Encoder" {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("Not available for dials.")
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    );
                    return;
                }

                ui.add_space(4.0);
                let old_action = self.sp_device_brightness_action.clone();
                ui.horizontal(|ui| {
                    ui.label("Action:");
                    egui::ComboBox::from_id_salt("sp_device_brightness_action")
                        .width(ui.available_width().min(220.0))
                        .selected_text(match self.sp_device_brightness_action.as_str() {
                            "increase" => "Increase",
                            "decrease" => "Decrease",
                            _ => "Set",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.sp_device_brightness_action,
                                "set".to_owned(),
                                "Set",
                            );
                            ui.selectable_value(
                                &mut self.sp_device_brightness_action,
                                "increase".to_owned(),
                                "Increase",
                            );
                            ui.selectable_value(
                                &mut self.sp_device_brightness_action,
                                "decrease".to_owned(),
                                "Decrease",
                            );
                        });
                });
                let changed_action = old_action != self.sp_device_brightness_action;

                ui.label(format!("Value ({}%)", self.sp_device_brightness_value));
                let changed_value = ui
                    .add(egui::Slider::new(
                        &mut self.sp_device_brightness_value,
                        0..=100,
                    ))
                    .changed();

                if changed_value || changed_action {
                    let ctx_to_set = instance.context.clone();
                    let settings = serde_json::json!({
                        "action": self.sp_device_brightness_action,
                        "value": self.sp_device_brightness_value,
                    });
                    self.spawn_set_instance_settings(ctx_to_set, settings);
                }
            }
            _ => {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new("No native editor for this action yet.")
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
            }
        }
    }

    #[allow(dead_code)]
    fn draw_action_editor_contents(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        device: &shared::DeviceInfo,
        snapshot: &ProfileSnapshot,
        selected_profile: &str,
        slot: &SelectedSlot,
    ) {
        let instance = match &slot.controller[..] {
            "Encoder" => snapshot
                .sliders
                .get(slot.position as usize)
                .and_then(|v| v.as_ref()),
            _ => snapshot
                .keys
                .get(slot.position as usize)
                .and_then(|v| v.as_ref()),
        };

        egui::Frame::group(ui.style())
            .corner_radius(egui::CornerRadius::same(12))
            .show(ui, |ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 8.0;

                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!("{} {}", slot.controller, slot.position))
                                .size(14.0)
                                .strong(),
                        );
                        ui.add_space(6.0);
                        if ui
                            .add_enabled(instance.is_some(), egui::Button::new("Clear"))
                            .on_hover_text("Remove the assigned action from this slot")
                            .clicked()
                        {
                            let ctx_to_clear = shared::Context {
                                device: device.id.clone(),
                                profile: selected_profile.to_owned(),
                                page: snapshot.page_id.clone(),
                                controller: slot.controller.clone(),
                                position: slot.position,
                            };
                            let _ = self.runtime.block_on(async {
                                riverdeck_core::api::instances::remove_instance(
                                    shared::ActionContext::from_context(ctx_to_clear, 0),
                                )
                                .await
                            });
                        }
                    });

                    ui.horizontal(|ui| {
                        if let Some(instance) = instance {
                            let icon_size = egui::vec2(28.0, 28.0);
                            let img = instance
                                .states
                                .get(instance.current_state as usize)
                                .map(|s| s.image.trim())
                                .filter(|s| !s.is_empty() && *s != "actionDefaultImage")
                                .map(|s| s.to_owned())
                                .unwrap_or_else(|| instance.action.icon.clone());
                            if let Some(tex) = self.texture_for_path(ctx, &img) {
                                ui.image((tex.id(), icon_size));
                            } else {
                                ui.allocate_exact_size(icon_size, egui::Sense::hover());
                            }
                            ui.label(instance.action.name.trim());
                        } else {
                            ui.label(
                                egui::RichText::new("No action assigned")
                                    .color(ui.visuals().weak_text_color()),
                            );
                        }
                    });

                    if let Some(instance) = instance {
                        let is_multi_action =
                            shared::is_multi_action_uuid(instance.action.uuid.as_str());
                        let has_native_options =
                            Self::native_options_kind(instance.action.uuid.as_str()).is_some();
                        let has_schema = riverdeck_core::options_schema::get_schema(
                            instance.action.uuid.as_str(),
                        )
                        .is_some();
                        if has_native_options {
                            self.ensure_options_editor_state(device, instance);
                        }
                        if has_schema {
                            self.ensure_schema_editor_state(instance);
                        }

                        // Keep editor state stable while switching slots.
                        let needs_reset = match self.button_label_context.as_ref() {
                            None => true,
                            Some(c) => c != &instance.context,
                        };
                        if needs_reset {
                            self.button_label_context = Some(instance.context.clone());
                            let st = instance.states.get(instance.current_state as usize);
                            self.button_label_input = st
                                .map(|s| s.text.clone())
                                .unwrap_or_else(|| instance.action.name.clone());
                            self.button_label_placement = st
                                .map(|s| s.text_placement)
                                .unwrap_or(shared::TextPlacement::Bottom);
                            self.button_show_title = st.map(|s| s.show).unwrap_or(true);
                            self.button_show_action_name =
                                st.map(|s| s.show_action_name).unwrap_or(true);
                        }

                        // Layout:
                        // - Normal actions: single full-width column
                        // - Multi/Toggle actions: two columns
                        let gap = 6.0;
                        let avail_w = ui.available_width().max(0.0);
                        let show_right = is_multi_action
                            || shared::is_toggle_action_uuid(instance.action.uuid.as_str())
                            || has_native_options
                            || has_schema;

                        if show_right {
                            // True split down the middle (matches the mental model in the UI).
                            let usable = (avail_w - gap).max(0.0);
                            let col_w = (usable * 0.5).max(0.0);
                            let left_w = col_w;
                            let right_w = col_w;
                            ui.horizontal(|ui| {
                                ui.allocate_ui_with_layout(
                                    egui::vec2(left_w, ui.available_height()),
                                    egui::Layout::top_down(egui::Align::Min),
                                    |ui| {
                                        self.draw_action_editor_left_column(
                                            ui, ctx, device, instance,
                                        )
                                    },
                                );
                                ui.add_space(gap);
                                ui.allocate_ui_with_layout(
                                    egui::vec2(right_w, ui.available_height()),
                                    egui::Layout::top_down(egui::Align::Min),
                                    |ui| {
                                        if is_multi_action {
                                            self.draw_action_editor_multi_action_right_column(
                                                ui, ctx, instance,
                                            );
                                        } else if has_schema {
                                            self.draw_action_editor_schema_right_column(
                                                ui, instance,
                                            );
                                        } else if has_native_options {
                                            self.draw_action_editor_native_options_right_column(
                                                ui, device, instance,
                                            );
                                        } else {
                                            // Toggle Action doesn't have a custom right panel (yet).
                                            ui.label(
                                                egui::RichText::new("Toggle Action")
                                                    .small()
                                                    .color(ui.visuals().weak_text_color()),
                                            );
                                        }
                                    },
                                );
                            });
                        } else {
                            ui.allocate_ui_with_layout(
                                egui::vec2(avail_w, ui.available_height()),
                                egui::Layout::top_down(egui::Align::Min),
                                |ui| self.draw_action_editor_left_column(ui, ctx, device, instance),
                            );
                        }
                    }
                });
            });

        // PI is intentionally not part of the default action editing flow anymore.
    }

    fn draw_action_editor_left_column(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        _device: &shared::DeviceInfo,
        instance: &shared::ActionInstance,
    ) {
        ui.add_space(6.0);
        ui.label("Button title (dynamic):");
        ui.add_sized(
            [ui.available_width(), 0.0],
            egui::TextEdit::singleline(&mut self.button_label_input)
                .hint_text("Leave empty for clean"),
        );

        ui.horizontal(|ui| {
            ui.checkbox(&mut self.button_show_action_name, "Show action name");
            ui.checkbox(&mut self.button_show_title, "Show title");
        });

        // This row can overflow in the single-column editor (e.g. "Auto (Top/Bottom)"),
        // which looks like the right side is cut off. Wrap instead of clipping.
        ui.horizontal_wrapped(|ui| {
            ui.label("Placement:");
            let placement_locked = self.button_show_title && self.button_show_action_name;
            ui.add_enabled_ui(!placement_locked, |ui| {
                let combo_w = ui.available_width().min(200.0);
                egui::ComboBox::from_id_salt("button_label_placement")
                    .width(combo_w)
                    .selected_text(match self.button_label_placement {
                        shared::TextPlacement::Top => "Top",
                        shared::TextPlacement::Bottom => "Bottom",
                        shared::TextPlacement::Left => "Left",
                        shared::TextPlacement::Right => "Right",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.button_label_placement,
                            shared::TextPlacement::Top,
                            "Top",
                        );
                        ui.selectable_value(
                            &mut self.button_label_placement,
                            shared::TextPlacement::Bottom,
                            "Bottom",
                        );
                        ui.selectable_value(
                            &mut self.button_label_placement,
                            shared::TextPlacement::Left,
                            "Left",
                        );
                        ui.selectable_value(
                            &mut self.button_label_placement,
                            shared::TextPlacement::Right,
                            "Right",
                        );
                    });
            });
            if placement_locked {
                ui.label(
                    egui::RichText::new("Auto (Top/Bottom)")
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
            }
        });

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button("Save").clicked()
                && let Some(ctx_to_set) = self.button_label_context.clone()
            {
                let text = self.button_label_input.clone();
                let placement = self.button_label_placement;
                let show_title = self.button_show_title;
                let show_action_name = self.button_show_action_name;
                self.runtime.spawn(async move {
                    let _ =
                        riverdeck_core::api::instances::set_button_label(ctx_to_set.clone(), text)
                            .await;
                    let _ = riverdeck_core::api::instances::set_button_label_placement(
                        ctx_to_set.clone(),
                        placement,
                    )
                    .await;
                    let _ = riverdeck_core::api::instances::set_button_show_title(
                        ctx_to_set.clone(),
                        show_title,
                    )
                    .await;
                    let _ = riverdeck_core::api::instances::set_button_show_action_name(
                        ctx_to_set,
                        show_action_name,
                    )
                    .await;
                });
            }
            ui.label(
                egui::RichText::new("Tip: empty label hides text")
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
        });

        ui.separator();

        // Wrap this row so it doesn't collapse the last label into 1-character columns
        // (which looks like "vertical text") when the editor is narrow.
        ui.horizontal_wrapped(|ui| {
            if ui.button("✕").on_hover_text("Remove custom icon").clicked() {
                let ctx_to_clear = instance.context.clone();
                let state = instance.current_state;
                self.runtime.spawn(async move {
                    let _ = riverdeck_core::api::instances::clear_custom_icon(
                        ctx_to_clear,
                        Some(state),
                    )
                    .await;
                });
            }
            if ui
                .button("Upload")
                .on_hover_text("Set custom icon")
                .clicked()
                && self.pending_icon_pick.is_none()
            {
                let (tx, rx) = mpsc::channel();
                std::thread::spawn(move || {
                    let picked = rfd::FileDialog::new()
                        .add_filter("Image", &["png", "jpg", "jpeg"])
                        .pick_file();
                    let _ = tx.send(picked);
                });
                self.pending_icon_pick =
                    Some((rx, instance.context.clone(), instance.current_state));
            }
            if ui
                .button("Icon Library…")
                .on_hover_text("Choose an icon from installed icon packs")
                .clicked()
            {
                self.icon_library_open = Some((instance.context.clone(), instance.current_state));
                self.icon_library_packs_cache = riverdeck_core::api::icon_packs::list_icon_packs();
                self.icon_library_selected_pack =
                    self.icon_library_packs_cache.first().map(|p| p.id.clone());
                self.icon_library_query.clear();
                self.icon_library_icons_cache.clear();
                self.icon_library_cache_key = None;
                self.icon_library_error = None;
            }
            if ui
                .button("Get more icons")
                .on_hover_text("Browse icon packs on the Elgato Marketplace")
                .clicked()
                && let Err(err) = self
                    .open_marketplace_url(ctx, "https://marketplace.elgato.com/stream-deck/icons")
            {
                self.marketplace_last_error = Some(err.to_string());
            }

            // No PI button: native editors live in the Action Editor.
            let has_native_options =
                Self::native_options_kind(instance.action.uuid.as_str()).is_some();
            if has_native_options {
                ui.label(
                    egui::RichText::new("Options are shown on the right.")
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
            } else if !instance.action.property_inspector.trim().is_empty() {
                if ui
                    .button("Open Property Inspector…")
                    .on_hover_text("Open this action's Property Inspector in a separate window")
                    .clicked()
                    && let Err(err) = self.open_property_inspector(ctx, instance)
                {
                    self.property_inspector_last_error = Some(err.to_string());
                }
                if let Some(err) = self.property_inspector_last_error.as_ref() {
                    ui.label(
                        egui::RichText::new(err)
                            .small()
                            .color(ui.visuals().error_fg_color),
                    );
                } else {
                    ui.label(
                        egui::RichText::new(
                            "This action uses a Property Inspector (opens in a separate window).",
                        )
                        .small()
                        .color(ui.visuals().weak_text_color()),
                    );
                }
            }
        });
    }

    fn draw_action_editor_multi_action_right_column(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        instance: &shared::ActionInstance,
    ) {
        ui.add_space(6.0);
        ui.label(egui::RichText::new("Multi Action").strong());

        let run_on = instance
            .settings
            .get("runOn")
            .and_then(|v| v.as_str())
            .unwrap_or("keyDown");
        let mut run_on_key_up = run_on == "keyUp";
        ui.horizontal(|ui| {
            ui.label("Run on:");
            egui::ComboBox::from_id_salt("multi_action_run_on")
                .width(ui.available_width().min(200.0))
                .selected_text(if run_on_key_up {
                    "Key Release"
                } else {
                    "Key Press"
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut run_on_key_up, false, "Key Press");
                    ui.selectable_value(&mut run_on_key_up, true, "Key Release");
                });
        });
        if (run_on == "keyUp") != run_on_key_up {
            let ctx_to_set = instance.context.clone();
            let run_on = if run_on_key_up {
                riverdeck_core::api::instances::MultiActionRunOn::KeyUp
            } else {
                riverdeck_core::api::instances::MultiActionRunOn::KeyDown
            };
            self.runtime.spawn(async move {
                let _ = riverdeck_core::api::instances::set_multi_action_run_on(ctx_to_set, run_on)
                    .await;
            });
        }

        ui.add_space(6.0);
        ui.label(egui::RichText::new("Steps").strong());
        if let Some(children) = instance.children.as_ref() {
            if children.is_empty() {
                ui.label(
                    egui::RichText::new(
                        "No steps yet. Drag actions onto this button to add steps.",
                    )
                    .small()
                    .color(ui.visuals().weak_text_color()),
                );
            } else {
                for (idx, child) in children.iter().enumerate() {
                    // Wrap if space gets tight so buttons never clip outside the panel.
                    ui.horizontal_wrapped(|ui| {
                        let icon_size = egui::vec2(20.0, 20.0);
                        let img = child
                            .states
                            .get(child.current_state as usize)
                            .map(|s| s.image.trim())
                            .filter(|s| !s.is_empty() && *s != "actionDefaultImage")
                            .map(|s| s.to_owned())
                            .unwrap_or_else(|| child.action.icon.clone());
                        if let Some(tex) = self.texture_for_path(ctx, &img) {
                            ui.image((tex.id(), icon_size));
                        } else {
                            ui.allocate_exact_size(icon_size, egui::Sense::hover());
                        }

                        ui.add(egui::Label::new(child.action.name.trim()).truncate());

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let btn_w = 22.0;
                            let btn = |text: &'static str| {
                                egui::Button::new(text).min_size(egui::vec2(btn_w, 0.0))
                            };

                            if ui.add(btn("✕")).on_hover_text("Remove step").clicked() {
                                let ctx_to_remove = child.context.clone();
                                self.runtime.spawn(async move {
                                    let _ = riverdeck_core::api::instances::remove_instance(
                                        ctx_to_remove,
                                    )
                                    .await;
                                });
                            }

                            let parent_ctx = instance.context.clone();
                            if ui.add_enabled(idx + 1 < children.len(), btn("↓")).clicked() {
                                let from = idx;
                                let to = idx + 1;
                                self.runtime.spawn(async move {
                                    let _ =
                                        riverdeck_core::api::instances::reorder_multi_action_child(
                                            parent_ctx, from, to,
                                        )
                                        .await;
                                });
                            }

                            let parent_ctx = instance.context.clone();
                            if ui.add_enabled(idx > 0, btn("↑")).clicked() {
                                let from = idx;
                                let to = idx - 1;
                                self.runtime.spawn(async move {
                                    let _ =
                                        riverdeck_core::api::instances::reorder_multi_action_child(
                                            parent_ctx, from, to,
                                        )
                                        .await;
                                });
                            }
                        });
                    });
                }
            }
        }

        ui.add_space(4.0);
        ui.add(
            egui::Label::new(
                egui::RichText::new("Tip: drag actions onto this button to add steps.")
                    .small()
                    .color(ui.visuals().weak_text_color()),
            )
            .wrap(),
        );
    }

    fn action_icon_path(action: &shared::Action) -> Option<&str> {
        // Prefer the first state's image if present (core already expands it to a concrete file path).
        if let Some(first) = action.states.first()
            && !first.image.trim().is_empty()
            && first.image != "actionDefaultImage"
        {
            return Some(first.image.as_str());
        }
        let icon = action.icon.trim();
        if icon.is_empty() { None } else { Some(icon) }
    }

    fn resolve_icon_path(&self, path: &str) -> Option<PathBuf> {
        let p = Path::new(path);
        if p.is_file() {
            return Some(p.to_path_buf());
        }

        if path.starts_with("riverdeck/") || path.starts_with("opendeck/") {
            let cfg = shared::config_dir().join(path);
            if cfg.is_file() {
                return Some(cfg);
            }
            if let Some(res) = shared::resource_dir() {
                let rp = res.join(path);
                if rp.is_file() {
                    return Some(rp);
                }
            }
        }

        None
    }

    fn texture_for_path(&mut self, ctx: &egui::Context, path: &str) -> Option<egui::TextureHandle> {
        let path = path.trim();

        // Support plugin-provided data URLs (common for `setImage`).
        if path.starts_with("data:") {
            use base64::Engine as _;
            use std::hash::{Hash, Hasher};

            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            path.hash(&mut hasher);
            let key = format!("riverdeck_data:{:x}", hasher.finish());

            if let Some(cached) = self.texture_cache.get(&key) {
                return Some(cached.texture.clone());
            }

            let bytes = if path.contains(";base64,") {
                let (_meta, b64) = path.split_once(";base64,")?;
                base64::engine::general_purpose::STANDARD.decode(b64).ok()?
            } else {
                let (_meta, raw) = path.split_once(',')?;
                raw.as_bytes().to_vec()
            };

            let img = image::load_from_memory(&bytes).ok()?.into_rgba8();
            let size = [img.width() as usize, img.height() as usize];
            let color_image = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());

            let texture = ctx.load_texture(
                format!("riverdeck_image:{key}"),
                color_image,
                egui::TextureOptions::LINEAR,
            );
            self.texture_cache.insert(
                key,
                CachedTexture {
                    modified: None,
                    texture: texture.clone(),
                },
            );
            return Some(texture);
        }

        // Core prefers `.svg` when present (see `shared::convert_icon`), but egui can't render SVG directly.
        // Best-effort fallback: if we get an `.svg` path, try a sibling `.png` / `@2x.png` instead.
        let (alt1, alt2) = if path.ends_with(".svg") {
            let base = path.trim_end_matches(".svg");
            (Some(format!("{base}@2x.png")), Some(format!("{base}.png")))
        } else {
            (None, None)
        };

        let mut candidates: Vec<&str> = Vec::with_capacity(3);
        candidates.push(path);
        if let Some(a) = alt1.as_deref() {
            candidates.push(a);
        }
        if let Some(a) = alt2.as_deref() {
            candidates.push(a);
        }

        for cand in candidates {
            // Now support SVG files by converting them on the fly
            let is_svg = cand.ends_with(".svg");
            if !(is_svg
                || cand.ends_with(".png")
                || cand.ends_with(".jpg")
                || cand.ends_with(".jpeg")
                || cand.ends_with(".gif"))
            {
                continue;
            }

            let Some(resolved) = self.resolve_icon_path(cand) else {
                continue;
            };
            let cache_key = resolved.to_string_lossy().into_owned();
            if !resolved.is_file() {
                self.texture_cache.remove(&cache_key);
                continue;
            }

            let modified = fs::metadata(&resolved).ok().and_then(|m| m.modified().ok());
            if let Some(cached) = self.texture_cache.get(&cache_key)
                && cached.modified == modified
            {
                return Some(cached.texture.clone());
            }

            let img = if is_svg {
                // Convert SVG to PNG using the same logic as elgato.rs
                let svg_data = std::fs::read(&resolved).ok()?;
                riverdeck_core::convert_svg_to_image(&svg_data)
                    .ok()?
                    .into_rgba8()
            } else {
                image::open(&resolved).ok()?.into_rgba8()
            };

            let size = [img.width() as usize, img.height() as usize];
            let color_image = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());

            let texture = ctx.load_texture(
                format!("riverdeck_image:{cache_key}"),
                color_image,
                egui::TextureOptions::LINEAR,
            );
            self.texture_cache.insert(
                cache_key,
                CachedTexture {
                    modified,
                    texture: texture.clone(),
                },
            );
            return Some(texture);
        }

        None
    }

    fn load_dynamic_image_for_preview(&self, path: &str) -> Option<image::DynamicImage> {
        let path = path.trim();
        if path.is_empty() {
            return None;
        }

        // Support plugin-provided data URLs (common for `setImage`).
        if path.starts_with("data:") {
            use base64::Engine as _;
            let bytes = if path.contains(";base64,") {
                let (_meta, b64) = path.split_once(";base64,")?;
                base64::engine::general_purpose::STANDARD.decode(b64).ok()?
            } else {
                let (_meta, raw) = path.split_once(',')?;
                raw.as_bytes().to_vec()
            };
            return image::load_from_memory(&bytes).ok();
        }

        // Best-effort SVG fallback (egui can't render SVG; neither can the Stream Deck).
        let (alt1, alt2) = if path.ends_with(".svg") {
            let base = path.trim_end_matches(".svg");
            (Some(format!("{base}@2x.png")), Some(format!("{base}.png")))
        } else {
            (None, None)
        };

        let mut candidates: Vec<&str> = Vec::with_capacity(3);
        candidates.push(path);
        if let Some(a) = alt1.as_deref() {
            candidates.push(a);
        }
        if let Some(a) = alt2.as_deref() {
            candidates.push(a);
        }

        for cand in candidates {
            // Now support SVG files by converting them on the fly
            let is_svg = cand.ends_with(".svg");
            if !(is_svg
                || cand.ends_with(".png")
                || cand.ends_with(".jpg")
                || cand.ends_with(".jpeg")
                || cand.ends_with(".gif"))
            {
                continue;
            }
            let resolved = self.resolve_icon_path(cand)?;
            if resolved.is_file() {
                if is_svg {
                    // Convert SVG to PNG
                    let svg_data = std::fs::read(&resolved).ok()?;
                    return riverdeck_core::convert_svg_to_image(&svg_data).ok();
                } else {
                    return image::open(resolved).ok();
                }
            }
        }
        None
    }

    fn rendered_texture_for_size(
        &mut self,
        ctx: &egui::Context,
        cache_ns: &str,
        width: u32,
        height: u32,
        base_image: &str,
        overlays: &[(String, shared::TextPlacement)],
    ) -> Option<egui::TextureHandle> {
        use std::hash::{Hash, Hasher};

        let base_image = base_image.trim();
        if base_image.is_empty() {
            return None;
        }

        // Resolve for cache invalidation if we can.
        let (resolved_path, modified) = if base_image.starts_with("data:") {
            (None, None)
        } else {
            let resolved = self.resolve_icon_path(base_image);
            let modified = resolved
                .as_ref()
                .and_then(|p| std::fs::metadata(p).ok())
                .and_then(|m| m.modified().ok());
            (resolved, modified)
        };

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        cache_ns.hash(&mut hasher);
        width.hash(&mut hasher);
        height.hash(&mut hasher);
        resolved_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| base_image.to_owned())
            .hash(&mut hasher);
        for (txt, placement) in overlays {
            txt.hash(&mut hasher);
            let p = match placement {
                shared::TextPlacement::Top => 0u8,
                shared::TextPlacement::Bottom => 1u8,
                shared::TextPlacement::Left => 2u8,
                shared::TextPlacement::Right => 3u8,
            };
            p.hash(&mut hasher);
        }

        let key = format!("riverdeck_rendered:{:x}", hasher.finish());
        if let Some(cached) = self.texture_cache.get(&key)
            && cached.modified == modified
        {
            return Some(cached.texture.clone());
        }

        let dyn_img = self.load_dynamic_image_for_preview(base_image)?;
        let mut img = dyn_img.resize_exact(width, height, image::imageops::FilterType::Nearest);
        for (label, placement) in overlays {
            img = riverdeck_core::render::label::overlay_label(img, label, *placement);
        }

        let rgba = img.into_rgba8();
        let size = [rgba.width() as usize, rgba.height() as usize];
        let color_image = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
        let texture = ctx.load_texture(
            format!("riverdeck_image:{key}"),
            color_image,
            egui::TextureOptions::NEAREST,
        );
        self.texture_cache.insert(
            key,
            CachedTexture {
                modified,
                texture: texture.clone(),
            },
        );
        Some(texture)
    }

    fn embedded_logo_texture(&mut self, ctx: &egui::Context) -> Option<egui::TextureHandle> {
        let cache_key = "riverdeck_embedded_logo".to_owned();
        if let Some(cached) = self.texture_cache.get(&cache_key) {
            return Some(cached.texture.clone());
        }

        let bytes = include_bytes!("../../../packaging/icons/icon.png");
        let img = image::load_from_memory(bytes).ok()?.into_rgba8();
        let size = [img.width() as usize, img.height() as usize];
        let color_image = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
        let texture = ctx.load_texture(
            "riverdeck_embedded_logo",
            color_image,
            egui::TextureOptions::LINEAR,
        );
        self.texture_cache.insert(
            cache_key,
            CachedTexture {
                modified: None,
                texture: texture.clone(),
            },
        );
        Some(texture)
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_screen_strip(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        device: &shared::DeviceInfo,
        snapshot: &ProfileSnapshot,
        selected_profile: &str,
        left_pad: f32,
        grid_width: f32,
    ) {
        let Some(screen) = device.screen.as_ref() else {
            return;
        };
        if screen.segments == 0 {
            return;
        }
        if !matches!(
            screen.placement,
            shared::ScreenPlacement::BetweenKeypadAndEncoders
        ) {
            return;
        }

        // Stream Deck+ UX: render the strip as contiguous, square-ish segments
        // (feels like one screen split into 4 parts).
        let segs = screen.segments as usize;
        let seg_w = (grid_width / segs as f32).max(1.0);
        // Half-height strip (relative to segment width).
        let strip_height = seg_w * 0.5;

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add_space(left_pad);
            let (rect, resp) =
                ui.allocate_exact_size(egui::vec2(grid_width, strip_height), egui::Sense::click());
            let painter = ui.painter_at(rect);

            // Background: either a user-selected bar background, or theme fill.
            if let Some(bg) = snapshot.encoder_screen_background.as_deref()
                && let Some(tex) = self.texture_for_path(ctx, bg)
            {
                painter.image(
                    tex.id(),
                    rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            } else {
                painter.rect_filled(
                    rect,
                    egui::CornerRadius::same(0),
                    ui.visuals().faint_bg_color,
                );
            }
            painter.rect_stroke(
                rect,
                egui::CornerRadius::same(0),
                ui.visuals().widgets.inactive.bg_stroke,
                egui::StrokeKind::Inside,
            );

            resp.context_menu(|ui| {
                if ui.button("Set background…").clicked() {
                    ui.close_menu();
                    if self.pending_screen_bg_pick.is_none() {
                        let (tx, rx) = mpsc::channel();
                        std::thread::spawn(move || {
                            let picked = rfd::FileDialog::new()
                                .add_filter("Image", &["png", "jpg", "jpeg", "gif"])
                                .pick_file();
                            let _ = tx.send(picked);
                        });
                        self.pending_screen_bg_pick =
                            Some((rx, device.id.clone(), selected_profile.to_owned()));
                    }
                }
                if ui.button("Clear background").clicked() {
                    ui.close_menu();
                    let device_id = device.id.clone();
                    let profile_id = selected_profile.to_owned();
                    self.runtime.spawn(async move {
                        let _ = riverdeck_core::api::profiles::set_encoder_screen_background(
                            device_id, profile_id, None,
                        )
                        .await;
                    });
                }
            });

            for i in 0..segs {
                let seg_rect = egui::Rect::from_min_max(
                    egui::pos2(rect.min.x + seg_w * i as f32, rect.min.y),
                    egui::pos2(rect.min.x + seg_w * (i as f32 + 1.0), rect.max.y),
                );

                // Segment border highlight when that encoder is selected.
                let selected = self
                    .selected_slot
                    .as_ref()
                    .is_some_and(|s| s.controller == "Encoder" && s.position as usize == i);
                let stroke = if selected {
                    egui::Stroke::new(2.0, ui.visuals().selection.stroke.color)
                } else {
                    egui::Stroke::new(1.0, ui.visuals().widgets.inactive.bg_stroke.color)
                };
                // Draw boundary lines between segments to create the “split screen” look.
                if i > 0 {
                    let x = rect.min.x + seg_w * i as f32;
                    painter.line_segment(
                        [egui::pos2(x, rect.min.y), egui::pos2(x, rect.max.y)],
                        ui.visuals().widgets.inactive.bg_stroke,
                    );
                }
                // Draw selected outline on top of boundaries.
                if selected {
                    painter.rect(
                        seg_rect,
                        egui::CornerRadius::same(0),
                        egui::Color32::TRANSPARENT,
                        stroke,
                        egui::StrokeKind::Inside,
                    );
                }

                let instance = snapshot.sliders.get(i).and_then(|v| v.as_ref());
                if let Some(instance) = instance {
                    let state_img = instance
                        .states
                        .get(instance.current_state as usize)
                        .map(|s| s.image.as_str())
                        .unwrap_or(instance.action.icon.as_str());
                    let img = state_img.trim();
                    let img = if img.is_empty() || img == "actionDefaultImage" {
                        instance.action.icon.as_str()
                    } else {
                        img
                    };

                    if let Some(tex) = self.texture_for_path(ctx, img) {
                        // Keep dial LCD icons square (don't stretch to the segment's wide aspect).
                        // The strip segments are wide+short; we inscribe a centered square and draw
                        // the image into that square.
                        let max_rect = seg_rect.shrink(8.0);
                        let side = max_rect.width().min(max_rect.height());
                        let square_rect =
                            egui::Rect::from_center_size(max_rect.center(), egui::vec2(side, side));
                        painter.image(
                            tex.id(),
                            square_rect,
                            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            egui::Color32::WHITE,
                        );
                    }
                } else {
                    painter.text(
                        seg_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        format!("Dial {}", i + 1),
                        egui::FontId::proportional(11.0),
                        ui.visuals().weak_text_color(),
                    );
                }
            }
        });
    }

    fn close_marketplace(&mut self) {
        if let Some(mut child) = self.marketplace_child.take() {
            riverdeck_core::runtime_processes::unrecord_process(child.id());
            let _ = child.kill();
            // Don't block the UI thread on `wait()` (can hang); reap on a background thread.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }

    fn close_property_inspector(&mut self) {
        if let Some(old_ctx) = self.property_inspector_context.take() {
            self.runtime.spawn(async move {
                riverdeck_core::api::property_inspector::switch_property_inspector(
                    Some(old_ctx),
                    None,
                )
                .await;
            });
        }
        if let Some(mut child) = self.property_inspector_child.take() {
            riverdeck_core::runtime_processes::unrecord_process(child.id());
            let _ = child.kill();
            // Don't block the UI thread on `wait()` (can hang); reap on a background thread.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }

    fn compute_property_inspector_geometry(ctx: &egui::Context) -> Option<(i32, i32, i32, i32)> {
        let outer = ctx.input(|i| i.viewport().outer_rect)?;
        let desired_w: f32 = 520.0;
        let desired_h: f32 = 720.0;
        let w = desired_w.min(outer.width().max(300.0)).round() as i32;
        let h = desired_h.min(outer.height().max(240.0)).round() as i32;
        // Prefer docking near the right side of the main window.
        let x = (outer.right() - (w as f32) - 12.0).round() as i32;
        let y = (outer.center().y - (h as f32 / 2.0)).round() as i32;
        Some((x.max(0), y.max(0), w.max(300), h.max(240)))
    }

    fn open_property_inspector(
        &mut self,
        ctx: &egui::Context,
        instance: &shared::ActionInstance,
    ) -> anyhow::Result<()> {
        let pi_rel = instance.action.property_inspector.trim();
        if pi_rel.is_empty() {
            return Err(anyhow::anyhow!("action has no property inspector"));
        }

        // Close any existing PI window so we don't orphan multiple windows.
        // This keeps the UX closer to the built-in Action Editor (one active editor at a time).
        self.close_property_inspector();

        let plugin_id = instance.action.plugin.trim().to_owned();
        if plugin_id.is_empty() {
            return Err(anyhow::anyhow!("missing plugin id for action"));
        }
        let plugin_dir = riverdeck_core::shared::config_dir()
            .join("plugins")
            .join(&plugin_id);
        let pi_path = plugin_dir.join(pi_rel);
        if !pi_path.exists() {
            return Err(anyhow::anyhow!(
                "property inspector not found at {}",
                pi_path.display()
            ));
        }

        // Load PI HTML via the local plugin webserver, with RiverDeck's injection suffix.
        let token = riverdeck_core::plugins::property_inspector_token();
        let ws_port = *riverdeck_core::plugins::PORT_BASE;
        let web_port = ws_port + 2;
        let raw_path = format!("{}|riverdeck_property_inspector", pi_path.to_string_lossy());
        let encoded = urlencoding::encode(&raw_path);
        let pi_src = format!("http://127.0.0.1:{web_port}/{encoded}?riverdeck_token={token}");

        // Stream Deck-compatible `info` payload for the PI.
        let info = self.runtime.block_on(async {
            riverdeck_core::api::property_inspector::make_info(plugin_id.clone()).await
        })?;
        let info_json = serde_json::to_string(&info)?;

        // Stream Deck-compatible "actionInfo" payload.
        let coords = (|| {
            let device = shared::DEVICES.get(&instance.context.device)?;
            if instance.context.controller != "Keypad" || device.columns == 0 {
                return None;
            }
            let pos = instance.context.position as u32;
            let cols = device.columns as u32;
            Some(serde_json::json!({
                "column": (pos % cols) as u8,
                "row": (pos / cols) as u8
            }))
        })();
        let connect_payload = serde_json::json!({
            "action": instance.action.uuid,
            "context": instance.context.to_string(),
            "device": instance.context.device,
            "payload": {
                "settings": instance.settings,
                "state": instance.current_state,
                "coordinates": coords,
                "isInMultiAction": false
            }
        });
        let connect_json = serde_json::to_string(&connect_payload)?;

        let label = format!("Property Inspector — {}", instance.action.name.trim());
        let dock = Self::compute_property_inspector_geometry(ctx);
        let child = match spawn_pi_process(
            &label,
            &pi_src,
            "*",
            ws_port,
            &instance.context.to_string(),
            &info_json,
            &connect_json,
            dock,
        ) {
            Ok(child) => child,
            Err(err) if is_process_not_found(&err) => {
                return Err(anyhow::anyhow!(
                    "property inspector helper not found (riverdeck-pi): {err:#}"
                ));
            }
            Err(err) => return Err(err),
        };

        let old = self.property_inspector_context.take();
        let new = instance.context.clone();
        self.property_inspector_context = Some(new.clone());
        self.property_inspector_child = Some(child);
        self.property_inspector_last_error = None;
        self.runtime.spawn(async move {
            riverdeck_core::api::property_inspector::switch_property_inspector(old, Some(new))
                .await;
        });
        Ok(())
    }

    fn compute_marketplace_geometry(ctx: &egui::Context) -> Option<(i32, i32, i32, i32)> {
        let outer = ctx.input(|i| i.viewport().outer_rect)?;
        let desired_w: f32 = 980.0;
        let desired_h: f32 = 720.0;
        let w = desired_w.min(outer.width().max(300.0)).round() as i32;
        let h = desired_h.min(outer.height().max(240.0)).round() as i32;
        let x = (outer.center().x - (w as f32 / 2.0)).round() as i32;
        let y = (outer.center().y - (h as f32 / 2.0)).round() as i32;
        Some((x.max(0), y.max(0), w.max(300), h.max(240)))
    }

    fn open_marketplace_url(
        &mut self,
        ctx: &egui::Context,
        marketplace_url: &str,
    ) -> anyhow::Result<()> {
        // If already open, do nothing (best effort).
        if self.marketplace_child.is_some() {
            return Ok(());
        }

        // Open the marketplace in a dedicated webview window.
        // Note: marketplaces may use custom deep links (e.g. `streamdeck://...`, `opendeck://...`).
        // We rely on the webview's navigation handler (in `riverdeck-pi`) + RiverDeck's startup
        // arg handler.
        let dock = Self::compute_marketplace_geometry(ctx);

        let child = match spawn_web_process("Marketplace", marketplace_url, dock) {
            Ok(child) => child,
            Err(err) if is_process_not_found(&err) => {
                // Dev/packaging fallback: if the webview helper isn't available, open in browser
                // instead of showing a confusing "No such file or directory" error.
                log::warn!("marketplace webview helper not found; opening in browser: {err:?}");
                open_url_in_browser(marketplace_url);
                return Ok(());
            }
            Err(err) => return Err(err),
        };

        self.marketplace_child = Some(child);
        self.marketplace_last_error = None;
        Ok(())
    }

    fn open_marketplace(&mut self, ctx: &egui::Context) -> anyhow::Result<()> {
        self.open_marketplace_url(ctx, riverdeck_core::openaction_marketplace::MARKETPLACE_URL)
    }

    fn draw_action_row(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        action: &shared::Action,
        row_size: egui::Vec2,
        dragging: bool,
        plugin_name: Option<&str>,
    ) -> egui::Response {
        let (rect, resp) = ui.allocate_exact_size(row_size, egui::Sense::click_and_drag());
        let painter = ui.painter_at(rect);

        let visuals = ui.visuals();
        let hovered = resp.hovered();
        let fill = if hovered {
            visuals.widgets.hovered.bg_fill
        } else {
            visuals.widgets.inactive.bg_fill
        };
        let stroke = if dragging {
            egui::Stroke::new(2.0, visuals.selection.stroke.color)
        } else if hovered {
            visuals.widgets.hovered.bg_stroke
        } else {
            visuals.widgets.inactive.bg_stroke
        };

        painter.rect(
            rect,
            egui::CornerRadius::same(10),
            fill,
            stroke,
            egui::StrokeKind::Inside,
        );

        // Icon on left
        let icon_size = 28.0;
        let icon_rect = egui::Rect::from_center_size(
            egui::pos2(rect.min.x + 16.0 + icon_size / 2.0, rect.center().y),
            egui::vec2(icon_size, icon_size),
        );
        if let Some(path) = Self::action_icon_path(action)
            && let Some(tex) = self.texture_for_path(ctx, path)
        {
            painter.image(
                tex.id(),
                icon_rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        } else {
            painter.text(
                icon_rect.center(),
                egui::Align2::CENTER_CENTER,
                "◻",
                egui::FontId::proportional(14.0),
                visuals.weak_text_color(),
            );
        }

        // Text to the right
        let text_left = icon_rect.max.x + 10.0;
        let text_rect = egui::Rect::from_min_max(
            egui::pos2(text_left, rect.min.y),
            egui::pos2(rect.max.x - 10.0, rect.max.y),
        );

        // If we have a plugin name, show action name on top and plugin name below
        if let Some(plugin) = plugin_name {
            let action_name_y = text_rect.center().y - 7.0;
            let plugin_name_y = text_rect.center().y + 7.0;

            painter.text(
                egui::pos2(text_rect.min.x, action_name_y),
                egui::Align2::LEFT_CENTER,
                action.name.trim(),
                egui::FontId::proportional(13.0),
                visuals.text_color(),
            );

            painter.text(
                egui::pos2(text_rect.min.x, plugin_name_y),
                egui::Align2::LEFT_CENTER,
                plugin,
                egui::FontId::proportional(10.0),
                visuals.weak_text_color(),
            );
        } else {
            // Single line centered if no plugin name
            painter.text(
                egui::pos2(text_rect.min.x, text_rect.center().y),
                egui::Align2::LEFT_CENTER,
                action.name.trim(),
                egui::FontId::proportional(13.0),
                visuals.text_color(),
            );
        }

        if !action.tooltip.trim().is_empty() {
            resp.on_hover_text(action.tooltip.trim())
        } else {
            resp
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_slot_preview(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        size: egui::Vec2,
        device: &shared::DeviceInfo,
        slot: &SelectedSlot,
        instance: Option<&shared::ActionInstance>,
        selected: bool,
        drag_payload: Option<&DragPayload>,
    ) -> egui::Response {
        let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
        let painter = ui.painter_at(rect);

        let pointer_pos = ctx.input(|i| i.pointer.latest_pos());
        let drop_hovered = pointer_pos.is_some_and(|p| rect.contains(p));
        let drop_valid = match drag_payload {
            Some(DragPayload::Action(a)) => {
                let controller_ok = a.controllers.iter().any(|c| c == &slot.controller);
                let slot_accepts = match instance {
                    None => true,
                    Some(inst) => {
                        // Only Multi/Toggle actions accept child actions.
                        let parent_uuid = inst.action.uuid.as_str();
                        let parent_accepts = shared::is_multi_action_uuid(parent_uuid)
                            || shared::is_toggle_action_uuid(parent_uuid);
                        if !parent_accepts {
                            false
                        } else {
                            // Enforce per-action `SupportedInMultiActions` and disallow nesting Multi/Toggle.
                            let action_is_builtin_multi =
                                shared::is_multi_action_uuid(a.uuid.as_str())
                                    || shared::is_toggle_action_uuid(a.uuid.as_str());
                            inst.children.is_some()
                                && a.supported_in_multi_actions
                                && !action_is_builtin_multi
                        }
                    }
                };
                controller_ok && slot_accepts
            }
            Some(DragPayload::Slot { slot: source, .. }) => source.controller == slot.controller,
            None => false,
        };
        if drag_payload.is_some() && drop_hovered {
            self.drag_hover_slot = Some(slot.clone());
            self.drag_hover_valid = drop_valid;
        }

        let stroke = if selected || (drag_payload.is_some() && drop_hovered && drop_valid) {
            egui::Stroke::new(2.0, ui.visuals().selection.stroke.color)
        } else if drag_payload.is_some() && drop_hovered && !drop_valid {
            egui::Stroke::new(2.0, ui.visuals().error_fg_color)
        } else {
            ui.visuals().widgets.inactive.bg_stroke
        };
        let fill = if drag_payload.is_some() && drop_hovered {
            ui.visuals().widgets.hovered.bg_fill
        } else {
            ui.visuals().extreme_bg_color
        };
        painter.rect(
            rect,
            egui::CornerRadius::same(10),
            fill,
            stroke,
            egui::StrokeKind::Inside,
        );

        if let Some(instance) = instance {
            let st = instance
                .states
                .get(instance.current_state as usize)
                .or_else(|| instance.states.first());
            let mut state_img = st
                .map(|s| s.image.trim())
                .filter(|s| !s.is_empty())
                .unwrap_or(instance.action.icon.trim());
            if state_img == "actionDefaultImage" {
                state_img = instance.action.icon.trim();
            }

            let overlays =
                riverdeck_core::events::outbound::devices::overlays_for_instance(instance)
                    .unwrap_or_default();
            let (kw, kh) =
                riverdeck_core::render::device::key_image_size_for_device_type(device.r#type);
            if let Some(tex) =
                self.rendered_texture_for_size(ctx, "keypad", kw, kh, state_img, &overlays)
            {
                let inner = rect.shrink(2.0);
                painter.image(
                    tex.id(),
                    inner,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            } else if let Some(tex) = self.texture_for_path(ctx, state_img) {
                let inner = rect.shrink(6.0);
                painter.image(
                    tex.id(),
                    inner,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            } else {
                painter.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    &instance.action.name,
                    egui::FontId::proportional(12.0),
                    ui.visuals().text_color(),
                );
            }
        } else {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                format!("{} {}", slot.controller, slot.position),
                egui::FontId::monospace(11.0),
                ui.visuals().weak_text_color(),
            );
        }

        resp
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_encoder_dial_preview(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        size: egui::Vec2,
        device: &shared::DeviceInfo,
        slot: &SelectedSlot,
        instance: Option<&shared::ActionInstance>,
        selected: bool,
        drag_payload: Option<&DragPayload>,
    ) -> egui::Response {
        let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
        let painter = ui.painter_at(rect);

        let pointer_pos = ctx.input(|i| i.pointer.latest_pos());
        let drop_hovered = pointer_pos.is_some_and(|p| rect.contains(p));
        let drop_valid = match drag_payload {
            Some(DragPayload::Action(a)) => {
                let controller_ok = a.controllers.iter().any(|c| c == &slot.controller);
                let slot_accepts = match instance {
                    None => true,
                    Some(inst) => {
                        let parent_uuid = inst.action.uuid.as_str();
                        let parent_accepts = shared::is_multi_action_uuid(parent_uuid)
                            || shared::is_toggle_action_uuid(parent_uuid);
                        if !parent_accepts {
                            false
                        } else {
                            let action_is_builtin_multi =
                                shared::is_multi_action_uuid(a.uuid.as_str())
                                    || shared::is_toggle_action_uuid(a.uuid.as_str());
                            inst.children.is_some()
                                && a.supported_in_multi_actions
                                && !action_is_builtin_multi
                        }
                    }
                };
                controller_ok && slot_accepts
            }
            Some(DragPayload::Slot { slot: source, .. }) => source.controller == slot.controller,
            None => false,
        };
        if drag_payload.is_some() && drop_hovered {
            self.drag_hover_slot = Some(slot.clone());
            self.drag_hover_valid = drop_valid;
        }

        let visuals = ui.visuals();
        let center = rect.center();
        let radius = (rect.width().min(rect.height()) * 0.5) - 2.0;

        let stroke = if selected || (drag_payload.is_some() && drop_hovered && drop_valid) {
            egui::Stroke::new(2.0, visuals.selection.stroke.color)
        } else if drag_payload.is_some() && drop_hovered && !drop_valid {
            egui::Stroke::new(2.0, visuals.error_fg_color)
        } else {
            visuals.widgets.inactive.bg_stroke
        };
        let fill = if drag_payload.is_some() && drop_hovered {
            visuals.widgets.hovered.bg_fill
        } else {
            visuals.extreme_bg_color
        };

        // Dial body
        painter.circle_filled(center, radius, fill);
        painter.circle_stroke(center, radius, stroke);

        // Optional: render action image/icon in an inscribed square.
        if let Some(instance) = instance {
            let st = instance
                .states
                .get(instance.current_state as usize)
                .or_else(|| instance.states.first());
            let mut state_img = st
                .map(|s| s.image.trim())
                .filter(|s| !s.is_empty())
                .unwrap_or(instance.action.icon.trim());
            if state_img == "actionDefaultImage" {
                state_img = instance.action.icon.trim();
            }

            let overlays =
                riverdeck_core::events::outbound::devices::overlays_for_instance(instance)
                    .unwrap_or_default();
            // Encoder icons are rendered at 72x72 on Stream Deck+ (the only encoder device today).
            let (iw, ih) = if device.r#type == 7 {
                (72u32, 72u32)
            } else {
                riverdeck_core::render::device::key_image_size_for_device_type(device.r#type)
            };
            if let Some(tex) =
                self.rendered_texture_for_size(ctx, "encoder", iw, ih, state_img, &overlays)
            {
                // Inscribed square inside circle (with padding) so it visually reads as a dial.
                let inner = rect.shrink(rect.width().min(rect.height()) * 0.22);
                painter.image(
                    tex.id(),
                    inner,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            } else if let Some(tex) = self.texture_for_path(ctx, state_img) {
                // Inscribed square inside circle (with padding) so it visually reads as a dial.
                let inner = rect.shrink(rect.width().min(rect.height()) * 0.22);
                painter.image(
                    tex.id(),
                    inner,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            } else {
                painter.text(
                    center,
                    egui::Align2::CENTER_CENTER,
                    &instance.action.name,
                    egui::FontId::proportional(12.0),
                    visuals.text_color(),
                );
            }
        } else {
            painter.text(
                center,
                egui::Align2::CENTER_CENTER,
                format!("Dial {}", slot.position + 1),
                egui::FontId::monospace(11.0),
                visuals.weak_text_color(),
            );
        }

        // Small indicator notch at the top for dial affordance.
        let notch_len = radius * 0.18;
        let notch_y = center.y - radius + 4.0;
        painter.line_segment(
            [
                egui::pos2(center.x, notch_y),
                egui::pos2(center.x, notch_y + notch_len),
            ],
            egui::Stroke::new(2.0, visuals.widgets.inactive.bg_stroke.color),
        );

        resp
    }

    fn load_profile_snapshot(
        &self,
        device: &shared::DeviceInfo,
        profile_id: &str,
    ) -> anyhow::Result<ProfileSnapshot> {
        self.runtime.block_on(async {
            let locks = riverdeck_core::store::profiles::acquire_locks().await;
            let store = locks.profile_stores.get_profile_store(device, profile_id)?;
            let page = store
                .value
                .pages
                .iter()
                .find(|p| p.id == store.value.selected_page)
                .or_else(|| store.value.pages.first());
            let page_id = page.map(|p| p.id.clone()).unwrap_or_else(|| "1".to_owned());
            Ok(ProfileSnapshot {
                page_id,
                keys: page.map(|p| p.keys.clone()).unwrap_or_default(),
                sliders: page.map(|p| p.sliders.clone()).unwrap_or_default(),
                encoder_screen_background: page.and_then(|p| p.encoder_screen_background.clone()),
            })
        })
    }

    fn poll_ui_events(&mut self) {
        let Some(rx) = self.ui_events.as_mut() else {
            return;
        };
        loop {
            match rx.try_recv() {
                Ok(event) => match event {
                    ui::UiEvent::PluginInstall { id, phase } => match phase {
                        ui::PluginInstallPhase::Started => {
                            self.toasts.set_installing(&id);
                        }
                        ui::PluginInstallPhase::Finished { ok, error } => {
                            self.toasts.finish_install(&id, ok, error);
                        }
                    },
                    _ => {
                        // For most events, the UI simply wakes; state is pulled from core singletons.
                    }
                },
                Err(tokio_mpsc::error::TryRecvError::Empty) => break,
                Err(tokio_mpsc::error::TryRecvError::Disconnected) => {
                    self.ui_events = None;
                    break;
                }
            }
        }
    }

    fn draw_toasts(&mut self, ctx: &egui::Context) {
        self.toasts.gc();
        if !self.toasts.is_active() {
            return;
        }

        #[derive(Clone)]
        enum ToastRef {
            Install(String),
            Completed(usize),
        }

        let mut list: Vec<(std::time::Instant, ToastRef)> = Vec::new();
        for (key, t) in self.toasts.install_toasts.iter() {
            list.push((t.created_at, ToastRef::Install(key.clone())));
        }
        for (idx, t) in self.toasts.completed.iter().enumerate() {
            list.push((t.created_at, ToastRef::Completed(idx)));
        }
        // Newest closest to the corner.
        list.sort_by_key(|(ts, _)| std::cmp::Reverse(*ts));

        let margin = 12.0;
        egui::Area::new(egui::Id::new("riverdeck_toasts"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-margin, -margin))
            .movable(false)
            .show(ctx, |ui| {
                ui.with_layout(egui::Layout::bottom_up(egui::Align::RIGHT), |ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(0.0, 10.0);
                    for (_ts, tref) in list {
                        let toast = match &tref {
                            ToastRef::Install(key) => self.toasts.install_toasts.get(key),
                            ToastRef::Completed(idx) => self.toasts.completed.get(*idx),
                        };
                        let Some(toast) = toast.cloned() else {
                            continue;
                        };

                        let accent = match toast.level {
                            ToastLevel::Info => egui::Color32::from_rgb(0, 145, 255),
                            ToastLevel::Success => egui::Color32::from_rgb(72, 199, 142),
                            ToastLevel::Error => ui.visuals().error_fg_color,
                        };

                        let mut close_requested = false;
                        egui::Frame::popup(ui.style())
                            .corner_radius(egui::CornerRadius::same(12))
                            .stroke(egui::Stroke::new(1.0, accent))
                            .inner_margin(egui::Margin::same(12))
                            .show(ui, |ui| {
                                ui.set_max_width(360.0);
                                ui.horizontal(|ui| {
                                    ui.label(
                                        egui::RichText::new(toast.title.clone())
                                            .strong()
                                            .color(ui.visuals().text_color()),
                                    );
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            let resp = ui.small_button("×");
                                            if resp.clicked() {
                                                close_requested = true;
                                            }
                                        },
                                    );
                                });
                                if let Some(body) = toast.body.clone()
                                    && !body.trim().is_empty()
                                {
                                    ui.add_space(4.0);
                                    ui.label(
                                        egui::RichText::new(body)
                                            .size(12.0)
                                            .color(ui.visuals().text_color()),
                                    );
                                }
                            });

                        if close_requested {
                            match tref {
                                ToastRef::Install(key) => {
                                    self.toasts.install_toasts.remove(&key);
                                }
                                ToastRef::Completed(idx) => {
                                    if idx < self.toasts.completed.len() {
                                        self.toasts.completed.remove(idx);
                                    }
                                }
                            }
                        }
                    }
                });
            });
    }
}

fn open_url_in_browser(url: &str) {
    use std::process::Command;
    #[cfg(target_os = "linux")]
    {
        let _ = Command::new("xdg-open").arg(url).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open").arg(url).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        // `start` is a shell built-in; `cmd /C start "" <url>` is the common pattern.
        let _ = Command::new("cmd").args(["/C", "start", "", url]).spawn();
    }
}

impl eframe::App for RiverDeckApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Professional dark theme pass: aim closer to the Stream Deck dark UI.
        ctx.style_mut(|s| {
            s.spacing.item_spacing = egui::vec2(12.0, 12.0);
            s.spacing.button_padding = egui::vec2(11.0, 9.0);
            s.spacing.window_margin = egui::Margin::same(14);

            let mut v = egui::Visuals::dark();
            // Surfaces
            v.window_fill = egui::Color32::from_rgb(24, 25, 27);
            v.panel_fill = egui::Color32::from_rgb(28, 29, 31);
            v.extreme_bg_color = egui::Color32::from_rgb(20, 21, 23);
            v.faint_bg_color = egui::Color32::from_rgb(34, 35, 38);

            // Widget cards
            v.widgets.inactive.bg_fill = egui::Color32::from_rgb(33, 34, 37);
            v.widgets.hovered.bg_fill = egui::Color32::from_rgb(41, 42, 46);
            v.widgets.active.bg_fill = egui::Color32::from_rgb(48, 50, 55);
            v.widgets.inactive.bg_stroke =
                egui::Stroke::new(1.0, egui::Color32::from_rgb(56, 58, 62));
            v.widgets.hovered.bg_stroke =
                egui::Stroke::new(1.0, egui::Color32::from_rgb(82, 85, 90));
            v.widgets.active.bg_stroke =
                egui::Stroke::new(1.0, egui::Color32::from_rgb(110, 114, 122));

            // Accents
            // Outline-only selection (avoid overpowering blue overlays on text-heavy widgets).
            v.selection.bg_fill = egui::Color32::TRANSPARENT;
            v.selection.stroke = egui::Stroke::new(2.0, egui::Color32::from_rgb(0, 145, 255));
            v.error_fg_color = egui::Color32::from_rgb(255, 95, 95);

            // Rounding
            v.window_corner_radius = egui::CornerRadius::same(12);
            v.menu_corner_radius = egui::CornerRadius::same(10);
            s.visuals = v;
        });

        // `tray-icon` on Linux uses GTK/AppIndicator under the hood, which relies on GLib to
        // process DBus registration events. eframe/winit does not run a GTK mainloop, so we
        // pump pending GLib events once per frame when tray support is enabled.
        #[cfg(all(target_os = "linux", feature = "tray"))]
        if self.tray.is_some() {
            // IMPORTANT: do not drain indefinitely. Some environments can keep the GLib main
            // context "always ready", which would stall the egui frame and prevent rendering.
            // A small cap per frame is enough to keep AppIndicator responsive.
            let ctx = gtk::glib::MainContext::default();
            for _ in 0..64 {
                if !ctx.iteration(false) {
                    break;
                }
            }
        }

        // If the user tries to close the window, prefer "hide to tray" (when available).
        // A real exit is done via the tray menu (Quit).
        #[cfg(feature = "tray")]
        if ctx.input(|i| i.viewport().close_requested()) {
            if QUIT_REQUESTED.load(Ordering::SeqCst) {
                // Allow the close to proceed.
            } else if self.hide_to_tray_available()
                && (cfg!(not(target_os = "linux")) || env_truthy("RIVERDECK_CLOSE_TO_TRAY"))
            {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.hide_to_tray_requested = true;
            }
        }

        if self.start_hidden {
            self.start_hidden = false;
            // If tray is available, start hidden-to-tray (no taskbar entry).
            // Otherwise, fall back to minimizing so the user can still find the app.
            #[cfg(feature = "tray")]
            if self.hide_to_tray_available() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
            #[cfg(not(feature = "tray"))]
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }

        #[cfg(feature = "tray")]
        if let Some(tray) = &mut self.tray {
            tray.poll(ctx);
        }

        // Allow external show requests (e.g. second instance wants to bring the window back).
        #[cfg(unix)]
        if SHOW_REQUESTED.swap(false, Ordering::SeqCst) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        }

        // Apply hide-to-tray requests (we do this after polling the tray menu so that a
        // "Show" click can't be immediately overridden by a stale request).
        #[cfg(feature = "tray")]
        if self.hide_to_tray_requested {
            self.hide_to_tray_requested = false;
            if self.hide_to_tray_available() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            } else {
                // Safer fallback: keep a taskbar entry so the user can restore the app.
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
        }

        self.poll_ui_events();

        // Poll async cache fetches without blocking the UI thread.
        // If any fetch is in-flight, we schedule a low-frequency repaint to pick up the result.
        let mut pending_async = false;
        if let Some(rx) = self.categories_rx.as_ref() {
            match rx.try_recv() {
                Ok(map) => {
                    let mut cats: Vec<_> = map.into_iter().collect();
                    cats.sort_by(|(a, _), (b, _)| a.cmp(b));
                    self.categories_cache = Some(cats);
                    self.categories_inflight = false;
                    self.categories_rx = None;
                    ctx.request_repaint();
                }
                Err(mpsc::TryRecvError::Empty) => pending_async = true,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.categories_inflight = false;
                    self.categories_rx = None;
                }
            }
        }
        if let Some(rx) = self.plugins_rx.as_ref() {
            match rx.try_recv() {
                Ok(list) => {
                    let mut list = list;
                    list.sort_by(|a, b| a.name.cmp(&b.name));
                    self.plugins_cache = Some(list);
                    self.plugins_inflight = false;
                    self.plugins_rx = None;
                    ctx.request_repaint();
                }
                Err(mpsc::TryRecvError::Empty) => pending_async = true,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.plugins_inflight = false;
                    self.plugins_rx = None;
                }
            }
        }
        if let Some(rx) = self.pages_rx.as_ref() {
            match rx.try_recv() {
                Ok((key, pages)) => {
                    self.pages_cache.insert(key.clone(), pages);
                    if self.pages_inflight.as_ref() == Some(&key) {
                        self.pages_inflight = None;
                    }
                    self.pages_rx = None;
                    ctx.request_repaint();
                }
                Err(mpsc::TryRecvError::Empty) => pending_async = true,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pages_inflight = None;
                    self.pages_rx = None;
                }
            }
        }

        // Complete any pending file picks without blocking the UI.
        if let Some((rx, context, state)) = self.pending_icon_pick.as_ref()
            && let Ok(picked) = rx.try_recv()
        {
            let context = context.clone();
            let state = *state;
            self.pending_icon_pick = None;
            if let Some(path) = picked {
                let path = path.to_string_lossy().into_owned();
                self.runtime.spawn(async move {
                    let _ = riverdeck_core::api::instances::set_custom_icon_from_path(
                        context,
                        Some(state),
                        path,
                    )
                    .await;
                });
            }
        }
        if let Some((rx, device_id, profile_id)) = self.pending_screen_bg_pick.as_ref()
            && let Ok(picked) = rx.try_recv()
        {
            let device_id = device_id.clone();
            let profile_id = profile_id.clone();
            self.pending_screen_bg_pick = None;
            if let Some(path) = picked {
                let path = path.to_string_lossy().into_owned();
                self.runtime.spawn(async move {
                    let _ = riverdeck_core::api::profiles::set_encoder_screen_background(
                        device_id,
                        profile_id,
                        Some(path),
                    )
                    .await;
                });
            }
        }

        // If we're waiting on async results, poll at a low frequency.
        if pending_async
            || self.pending_plugin_install_result.is_some()
            || self.pending_plugin_remove_result.is_some()
            || self.pending_icon_pick.is_some()
            || self.pending_screen_bg_pick.is_some()
            || self.toasts.is_active()
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        // Plugin management operations (non-blocking result channel).
        if let Some(rx) = self.pending_plugin_install_result.as_ref()
            && let Ok(res) = rx.try_recv()
        {
            self.pending_plugin_install_result = None;
            match res {
                Ok(()) => {
                    self.plugin_manage_error = None;
                    // Invalidate caches that depend on plugin discovery and refresh plugins.
                    self.plugins_cache = None;
                    self.plugins_inflight = false;
                    self.plugins_rx = None;
                    self.categories_cache = None;
                    self.categories_inflight = false;
                    self.categories_rx = None;
                    self.runtime.spawn(async {
                        plugins::initialise_plugins();
                    });
                }
                Err(err) => {
                    self.plugin_manage_error = Some(err);
                }
            }
        }

        if let Some(rx) = self.pending_plugin_remove_result.as_ref()
            && let Ok(res) = rx.try_recv()
        {
            self.pending_plugin_remove_result = None;
            match res {
                Ok(()) => {
                    self.plugin_manage_error = None;
                    // Invalidate caches that depend on plugin discovery and refresh plugins.
                    self.plugins_cache = None;
                    self.plugins_inflight = false;
                    self.plugins_rx = None;
                    self.categories_cache = None;
                    self.categories_inflight = false;
                    self.categories_rx = None;
                    self.runtime.spawn(async {
                        plugins::initialise_plugins();
                    });
                }
                Err(err) => {
                    self.plugin_manage_error = Some(err);
                }
            }
        }

        // Reset per-frame drag hover state.
        self.drag_hover_slot = None;
        self.drag_hover_valid = false;

        // Avoid borrow conflicts: take a local snapshot of the drag payload for this frame.
        let drag_payload = self.drag_payload.clone();

        egui::TopBottomPanel::top("top")
            // Keep the top bar tighter than the rest of the window so the window buttons
            // don't feel "floating" away from the right edge.
            .frame(egui::Frame::NONE.inner_margin(egui::Margin::same(8)))
            .show(ctx, |ui| {
                #[cfg(target_os = "linux")]
                {
                    self.draw_custom_titlebar(ui, ctx);
                }

                #[cfg(not(target_os = "linux"))]
                {
                    ui.horizontal(|ui| {
                        if let Some(tex) = self.embedded_logo_texture(ctx) {
                            ui.image((tex.id(), egui::vec2(20.0, 20.0)));
                        }
                        ui.heading("RiverDeck (egui)");
                        ui.separator();
                        ui.label(format!("devices: {}", shared::DEVICES.len()));
                        if let Some(info) = self.update_info.lock().unwrap().as_ref() {
                            ui.separator();
                            ui.label(format!("Update available: {}", info.tag));
                            if ui.button("Details").clicked() {
                                self.show_update_details = true;
                            }
                        }
                    });
                }
            });

        if self.show_update_details {
            if let Some(info) = self.update_info.lock().unwrap().clone() {
                let mut open = self.show_update_details;
                let mut close_requested = false;
                egui::Window::new(format!("Update available: {}", info.tag))
                    .open(&mut open)
                    .title_bar(false)
                    .frame(
                        egui::Frame::window(ctx.style().as_ref()).inner_margin(egui::Margin::ZERO),
                    )
                    .show(ctx, |ui| {
                        close_requested |= Self::draw_popup_titlebar(ui, "Update details");
                        egui::Frame::NONE
                            .inner_margin(egui::Margin::same(12))
                            .show(ui, |ui| {
                                egui::ScrollArea::vertical()
                                    .max_height(300.0)
                                    .show(ui, |ui| {
                                        ui.label(&info.body);
                                    });
                                ui.label("Update via your package manager / release download.");
                            });
                    });
                if close_requested {
                    open = false;
                }
                self.show_update_details = open;
            } else {
                self.show_update_details = false;
            }
        }

        egui::SidePanel::left("devices")
            .default_width(Self::DEVICES_PANEL_DEFAULT_WIDTH)
            .min_width(Self::DEVICES_PANEL_DEFAULT_WIDTH)
            .show(ctx, |ui| {
                // Top: devices
                ui.heading("Devices");
                ui.add_space(6.0);

                egui::Frame::group(ui.style())
                    .corner_radius(egui::CornerRadius::same(12))
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 6.0;
                        for entry in shared::DEVICES.iter() {
                            let id = entry.key().clone();
                            let selected = self.selected_device.as_deref() == Some(&id);
                            let row_height = 46.0;
                            let row_padding = egui::vec2(10.0, 7.0);

                            let (rect, resp) = ui.allocate_exact_size(
                                egui::vec2(ui.available_width(), row_height),
                                egui::Sense::click(),
                            );

                            let rounding = egui::CornerRadius::same(10);
                            let bg_fill = if resp.hovered() || resp.has_focus() {
                                ui.visuals().widgets.hovered.bg_fill
                            } else {
                                egui::Color32::TRANSPARENT
                            };
                            let stroke = if selected {
                                ui.visuals().selection.stroke
                            } else if resp.hovered() {
                                ui.visuals().widgets.hovered.bg_stroke
                            } else {
                                egui::Stroke::new(1.0, egui::Color32::TRANSPARENT)
                            };

                            ui.painter().rect(
                                rect,
                                rounding,
                                bg_fill,
                                stroke,
                                egui::StrokeKind::Inside,
                            );

                            // Important: paint text manually so the whole row stays a single hit target.
                            // (Widgets like `Label` can capture pointer input and make the row feel flaky.)
                            let inner = rect.shrink2(row_padding);
                            let painter = ui.painter_at(rect);

                            let name_font = egui::FontId::proportional(14.0);
                            let id_font = egui::FontId::proportional(11.0);

                            painter.text(
                                egui::pos2(inner.min.x, inner.min.y),
                                egui::Align2::LEFT_TOP,
                                entry.value().name.trim(),
                                name_font,
                                ui.visuals().text_color(),
                            );
                            painter.text(
                                egui::pos2(inner.min.x, inner.max.y),
                                egui::Align2::LEFT_BOTTOM,
                                id.trim(),
                                id_font,
                                ui.visuals().weak_text_color(),
                            );

                            if resp.clicked() {
                                self.selected_device = Some(id);
                                self.selected_slot = None;
                                self.action_editor_open = false;
                            }
                        }
                    });

                // Bottom-left: Marketplace section (use bottom-up layout so it never gets pushed
                // off-screen by large spacers).
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.add_space(4.0);
                    if ui.button("OpenAction Marketplace").clicked()
                        && let Err(err) = self.open_marketplace(ctx)
                    {
                        self.marketplace_last_error = Some(err.to_string());
                    }
                    ui.add_space(6.0);
                    if let Some(err) = self.marketplace_last_error.as_ref() {
                        ui.colored_label(ui.visuals().error_fg_color, err);
                    }
                    ui.separator();
                });
            });

        let selected_device = self
            .selected_device
            .as_ref()
            .and_then(|id| shared::DEVICES.get(id).map(|d| d.value().clone()));
        if self.selected_device.is_some() && selected_device.is_none() {
            self.selected_device = None;
            self.selected_slot = None;
            self.action_editor_open = false;
        }

        let selected_profile = selected_device
            .as_ref()
            .and_then(|device| {
                self.runtime
                    .block_on(async {
                        riverdeck_core::api::profiles::get_selected_profile(device.id.clone())
                            .await
                            .map(|p| p.id)
                    })
                    .ok()
            })
            .unwrap_or_else(|| "Default".to_owned());

        let snapshot = selected_device
            .as_ref()
            .and_then(|d| self.load_profile_snapshot(d, &selected_profile).ok());

        if let (Some(device), Some(slot)) = (&selected_device, &self.selected_slot) {
            let key_count = (device.rows as usize) * (device.columns as usize);
            if slot.controller == "Encoder" {
                if slot.position as usize >= device.encoders as usize {
                    self.selected_slot = None;
                    self.action_editor_open = false;
                }
            } else if slot.position as usize >= key_count {
                self.selected_slot = None;
                self.action_editor_open = false;
            }
        }

        egui::SidePanel::right("actions")
            .default_width(Self::ACTIONS_PANEL_DEFAULT_WIDTH)
            .min_width(Self::ACTIONS_PANEL_DEFAULT_WIDTH)
            .show(ctx, |ui| {
                ui.heading("Actions");

                let Some(device) = &selected_device else {
                    ui.label("Select a device.");
                    return;
                };

                // Controller filter (Keypad/Encoder) for the action list.
                egui::Frame::group(ui.style())
                    .corner_radius(egui::CornerRadius::same(12))
                    .show(ui, |ui| {
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.label("Show:");
                                ui.spacing_mut().item_spacing.x = 6.0;
                                ui.selectable_value(
                                    &mut self.action_controller_filter,
                                    "Keypad".to_owned(),
                                    egui::RichText::new("Keys"),
                                );
                                if device.encoders > 0 {
                                    ui.selectable_value(
                                        &mut self.action_controller_filter,
                                        "Encoder".to_owned(),
                                        egui::RichText::new("Dials"),
                                    );
                                }
                            });
                            ui.add_space(6.0);
                            ui.add(
                                egui::TextEdit::singleline(&mut self.action_search)
                                    .hint_text("Search actions…"),
                            );
                        });
                    });
                ui.separator();

                if self.categories_cache.is_none()
                    && !self.categories_inflight
                    && self.categories_rx.is_none()
                {
                    self.categories_inflight = true;
                    let (tx, rx) = mpsc::channel();
                    self.categories_rx = Some(rx);
                    self.runtime.spawn(async move {
                        let _ = tx.send(riverdeck_core::api::get_categories().await);
                    });
                }

                // Fetch plugins list if not already cached (needed for plugin names in action list)
                if self.plugins_cache.is_none()
                    && !self.plugins_inflight
                    && self.plugins_rx.is_none()
                {
                    self.plugins_inflight = true;
                    let (tx, rx) = mpsc::channel();
                    self.plugins_rx = Some(rx);
                    std::thread::spawn(move || {
                        let list = riverdeck_core::plugins::marketplace::list_local_plugins();
                        let _ = tx.send(list);
                    });
                }

                // Clone for this frame to avoid borrowing `self` across the ScrollArea closure
                // (we need `&mut self` inside for `draw_action_row`).
                let cats = self.categories_cache.clone().unwrap_or_default();

                // Build a map of plugin UUID -> plugin name for display
                let plugin_names: std::collections::HashMap<String, String> = self
                    .plugins_cache
                    .as_ref()
                    .map(|plugins| {
                        plugins
                            .iter()
                            .map(|p| (p.id.clone(), p.name.clone()))
                            .collect()
                    })
                    .unwrap_or_default();

                let search = self.action_search.to_lowercase();
                let filter_controller = self.action_controller_filter.clone();
                let row_height = 44.0;

                egui::ScrollArea::vertical().show(ui, |ui| {
                    if cats.is_empty() && (self.categories_inflight || self.categories_rx.is_some())
                    {
                        ui.label(
                            egui::RichText::new("Loading actions…")
                                .small()
                                .color(ui.visuals().weak_text_color()),
                        );
                    }
                    for (cat_name, cat) in cats {
                        egui::CollapsingHeader::new(cat_name)
                            .default_open(true)
                            .show(ui, |ui| {
                                ui.spacing_mut().item_spacing.y = 8.0;
                                for action in cat.actions.iter() {
                                    if !action.visible_in_action_list {
                                        continue;
                                    }
                                    if !action.controllers.iter().any(|c| c == &filter_controller) {
                                        continue;
                                    }
                                    if !search.is_empty()
                                        && !action.name.to_lowercase().contains(&search)
                                    {
                                        continue;
                                    }

                                    let dragging_this =
                                        self.drag_payload.as_ref().is_some_and(|p| match p {
                                            DragPayload::Action(a) => {
                                                a.uuid == action.uuid && a.plugin == action.plugin
                                            }
                                            _ => false,
                                        });
                                    let plugin_name =
                                        plugin_names.get(&action.plugin).map(|s| s.as_str());
                                    let row_size = egui::vec2(ui.available_width(), row_height);
                                    let resp = self.draw_action_row(
                                        ui,
                                        ctx,
                                        action,
                                        row_size,
                                        dragging_this,
                                        plugin_name,
                                    );
                                    if resp.drag_started() {
                                        self.drag_payload =
                                            Some(DragPayload::Action(action.clone()));
                                    }
                                }
                            });
                    }
                });
            });

        // Action editor: automatically shown when the selected slot has an action instance.
        // If no slot is selected (or the slot is empty), the editor is hidden.
        let editor_visible =
            selected_device.is_some() && snapshot.is_some() && self.selected_slot.is_some();
        if !editor_visible {
            self.action_editor_open = false;
        } else if let (Some(device), Some(snapshot), Some(slot)) = (
            &selected_device,
            snapshot.as_ref(),
            self.selected_slot.clone(),
        ) {
            let selected_instance = match &slot.controller[..] {
                "Encoder" => snapshot
                    .sliders
                    .get(slot.position as usize)
                    .and_then(|v| v.as_ref()),
                _ => snapshot
                    .keys
                    .get(slot.position as usize)
                    .and_then(|v| v.as_ref()),
            };
            let selected_uses_two_cols = selected_instance.is_some_and(|i| {
                shared::is_multi_action_uuid(i.action.uuid.as_str())
                    || shared::is_toggle_action_uuid(i.action.uuid.as_str())
                    || Self::native_options_kind(i.action.uuid.as_str()).is_some()
            });

            let instance_present = match &slot.controller[..] {
                "Encoder" => snapshot
                    .sliders
                    .get(slot.position as usize)
                    .and_then(|v| v.as_ref())
                    .is_some(),
                _ => snapshot
                    .keys
                    .get(slot.position as usize)
                    .and_then(|v| v.as_ref())
                    .is_some(),
            };

            // Keep the state in sync with selection: open iff the slot contains an action.
            self.action_editor_open = instance_present;

            let anim_t =
                ctx.animate_bool(egui::Id::new("action_editor_anim"), self.action_editor_open);
            if anim_t > 0.0 {
                // Size/center against the preview geometry (cached each frame from the preview area).
                let avail = ctx.available_rect();
                let screen = ctx.screen_rect();
                let preview_w = self.preview_width.unwrap_or(avail.width());
                // Pick width from the editor's content rather than from ad-hoc scaling factors.
                // This keeps the editor consistent across "button types" and only grows when the
                // editor actually shows more UI (e.g. Multi Action / Toggle Action).
                let col_min_w = 360.0;
                let cols_gap = 6.0;
                let content_min_w: f32 = if selected_uses_two_cols {
                    (col_min_w * 2.0) + cols_gap
                } else {
                    col_min_w
                };
                let max_w = preview_w.max(1.0);
                // Account for the popup frame inner margin so content doesn't get clipped.
                let frame_margin_i8: i8 = 10;
                let frame_margin = frame_margin_i8 as f32;
                let content_min_outer_w = content_min_w + (frame_margin * 2.0);
                let target_w = content_min_outer_w.min(max_w);

                // Keep height consistent across action types by basing it on the full window,
                // not on `available_rect()` (which varies as side panels/content change).
                let target_h = (screen.height() * 0.45).clamp(260.0, 460.0);
                let current_h = target_h * anim_t;

                // Flush to the bottom edge (no lift).
                let margin = 0.0;
                let center_x = self.preview_center_x.unwrap_or_else(|| avail.center().x);
                let x = center_x - (target_w * 0.5);
                let y = (screen.bottom() - margin - current_h).max(screen.top());
                let rect =
                    egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(target_w, current_h));
                // Clip slightly *outside* the overlay rect so the popup frame's stroke/rounding
                // (and any AA/shadow bleed) doesn't get cut off at the edges.
                let clip_rect = rect.expand(frame_margin + 24.0);

                egui::Area::new("action_editor_overlay".into())
                    .order(egui::Order::Foreground)
                    .fixed_pos(egui::pos2(x, y))
                    .show(ctx, |ui| {
                        // Size and clip the Area's UI directly. Using an absolute `max_rect(rect)`
                        // inside an already-positioned Area can lead to unexpected expansion.
                        ui.set_min_size(rect.size());
                        ui.set_max_size(rect.size());
                        ui.set_clip_rect(clip_rect);

                        egui::Frame::popup(ui.style())
                            .inner_margin(egui::Margin::same(frame_margin_i8))
                            .corner_radius(egui::CornerRadius::same(12))
                            .show(ui, |ui| {
                                // Avoid trying to render the full editor while we're still animating
                                // from ~0px tall (it would just clip awkwardly).
                                if current_h < 48.0 {
                                    return;
                                }

                                ui.vertical(|ui| {
                                    ui.heading("Action Editor");
                                    ui.add_space(6.0);

                                    egui::ScrollArea::vertical().show(ui, |ui| {
                                        self.draw_action_editor_contents(
                                            ui,
                                            ctx,
                                            device,
                                            snapshot,
                                            &selected_profile,
                                            &slot,
                                        );
                                    });
                                });
                            });
                    });
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            let Some(device) = &selected_device else {
                ui.label("Select a device to view details.");
                return;
            };

            egui::Frame::group(ui.style())
                .corner_radius(egui::CornerRadius::same(14))
                .show(ui, |ui| {
                    ui.heading(&device.name);
                    ui.label(
                        egui::RichText::new(&device.id)
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.label("Profile:");
                        // Load list for dropdown.
                        let mut profiles = riverdeck_core::api::profiles::get_profiles(&device.id)
                            .unwrap_or_else(|_| vec!["Default".to_owned()]);
                        profiles.sort();

                        egui::ComboBox::from_id_salt("profile_combo")
                            .selected_text(&selected_profile)
                            .show_ui(ui, |ui| {
                                for p in profiles.iter() {
                                    if ui.selectable_label(p == &selected_profile, p).clicked() {
                                        let device_id = device.id.clone();
                                        let p = p.clone();
                                        self.runtime.spawn(async move {
                                            let _ = riverdeck_core::api::profiles::set_selected_profile(device_id, p).await;
                                        });
                                    }
                                }
                            });

                        if ui.button("New…").clicked() {
                            self.show_profile_editor = true;
                            self.profile_name_input.clear();
                            self.profile_error = None;
                        }
                        if ui
                            .add_enabled(selected_profile != "Default", egui::Button::new("Rename…"))
                            .clicked()
                        {
                            self.show_profile_editor = true;
                            self.profile_name_input = selected_profile.clone();
                            self.profile_error = None;
                        }
                        if ui
                            .add_enabled(selected_profile != "Default", egui::Button::new("Delete"))
                            .clicked()
                        {
                            let device_id = device.id.clone();
                            let to_delete = selected_profile.clone();
                            self.runtime.spawn(async move {
                                riverdeck_core::api::profiles::delete_profile(device_id, to_delete).await;
                            });
                        }
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Reload plugins").clicked() {
                            // Must run inside a Tokio runtime (plugins::initialise_plugins uses tokio::spawn).
                            self.runtime.spawn(async {
                                plugins::initialise_plugins();
                            });
                        }
                        if ui.button("Manage plugins…").clicked() {
                            self.show_manage_plugins = true;
                            self.plugin_manage_error = None;
                        }
                    });
                });

            if self.show_profile_editor {
                let mut open = true;
                let mut close_requested = false;
                egui::Window::new("Profile")
                    .open(&mut open)
                    .title_bar(false)
                    .frame(egui::Frame::window(ctx.style().as_ref()).inner_margin(egui::Margin::ZERO))
                    .collapsible(false)
                    .resizable(false)
                    .show(ctx, |ui| {
                        close_requested |= Self::draw_popup_titlebar(ui, "Profile");
                        egui::Frame::NONE
                            .inner_margin(egui::Margin::same(12))
                            .show(ui, |ui| {
                                ui.label("Name:");
                                ui.add(egui::TextEdit::singleline(&mut self.profile_name_input));
                                if let Some(err) = self.profile_error.as_ref() {
                                    ui.colored_label(ui.visuals().error_fg_color, err);
                                }
                                ui.add_space(8.0);

                                let trimmed = self.profile_name_input.trim().to_owned();
                                let valid = !trimmed.is_empty();

                                ui.horizontal(|ui| {
                                    if ui
                                        .add_enabled(valid, egui::Button::new("Create (empty)"))
                                        .clicked()
                                    {
                                        let device_id = device.id.clone();
                                        let new_id = trimmed.clone();
                                        self.runtime.spawn(async move {
                                            let _ = riverdeck_core::api::profiles::create_profile(
                                                device_id.clone(),
                                                new_id.clone(),
                                                None,
                                            )
                                            .await;
                                            let _ = riverdeck_core::api::profiles::set_selected_profile(
                                                device_id,
                                                new_id,
                                            )
                                            .await;
                                        });
                                        self.show_profile_editor = false;
                                    }
                                    if ui
                                        .add_enabled(valid, egui::Button::new("Duplicate current"))
                                        .clicked()
                                    {
                                        let device_id = device.id.clone();
                                        let new_id = trimmed.clone();
                                        let from_id = selected_profile.clone();
                                        self.runtime.spawn(async move {
                                            let _ = riverdeck_core::api::profiles::create_profile(
                                                device_id.clone(),
                                                new_id.clone(),
                                                Some(from_id),
                                            )
                                            .await;
                                            let _ = riverdeck_core::api::profiles::set_selected_profile(
                                                device_id,
                                                new_id,
                                            )
                                            .await;
                                        });
                                        self.show_profile_editor = false;
                                    }
                                    if ui
                                        .add_enabled(
                                            valid && selected_profile != "Default",
                                            egui::Button::new("Rename"),
                                        )
                                        .clicked()
                                    {
                                        let device_id = device.id.clone();
                                        let old_id = selected_profile.clone();
                                        let new_id = trimmed.clone();
                                        self.runtime.spawn(async move {
                                            let _ = riverdeck_core::api::profiles::rename_profile(
                                                device_id.clone(),
                                                old_id,
                                                new_id.clone(),
                                            )
                                            .await;
                                            let _ = riverdeck_core::api::profiles::set_selected_profile(
                                                device_id,
                                                new_id,
                                            )
                                            .await;
                                        });
                                        self.show_profile_editor = false;
                                    }
                                    if ui.button("Cancel").clicked() {
                                        self.show_profile_editor = false;
                                    }
                                });
                            });
                    });
                if close_requested {
                    open = false;
                }
                if !open {
                    self.show_profile_editor = false;
                }
            }

            if self.show_settings {
                let mut open = true;
                let mut close_requested = false;
                egui::Window::new("Settings")
                    .open(&mut open)
                    .title_bar(false)
                    .frame(egui::Frame::window(ctx.style().as_ref()).inner_margin(egui::Margin::ZERO))
                    .collapsible(false)
                    .resizable(false)
                    .show(ctx, |ui| {
                        close_requested |= Self::draw_popup_titlebar(ui, "Settings");
                        egui::Frame::NONE
                            .inner_margin(egui::Margin::same(12))
                            .show(ui, |ui| {
                                if let Some(err) = self.settings_error.as_ref() {
                                    ui.colored_label(ui.visuals().error_fg_color, err);
                                }

                                let mut changed = false;
                                changed |= ui
                                    .checkbox(&mut self.settings_autostart, "Autostart")
                                    .changed();
                                changed |= ui
                                    .checkbox(
                                        &mut self.settings_screensaver,
                                        "Screensaver (placeholder)",
                                    )
                                    .changed();

                                if changed {
                                    match riverdeck_core::store::get_settings() {
                                        Ok(mut store) => {
                                            store.value.autolaunch = self.settings_autostart;
                                            store.value.screensaver = self.settings_screensaver;
                                            if let Err(err) = store.save() {
                                                self.settings_error = Some(err.to_string());
                                            } else {
                                                self.settings_error = None;
                                                // Apply autostart immediately.
                                                configure_autostart();
                                            }
                                        }
                                        Err(err) => self.settings_error = Some(err.to_string()),
                                    }
                                }
                            });
                    });
                if close_requested {
                    open = false;
                }
                if !open {
                    self.show_settings = false;
                }
            }

            if self.show_manage_plugins {
                let mut open = true;
                let mut close_requested = false;
                egui::Window::new("Manage Plugins")
                    .open(&mut open)
                    .title_bar(false)
                    .frame(egui::Frame::window(ctx.style().as_ref()).inner_margin(egui::Margin::ZERO))
                    .collapsible(false)
                    .resizable(true)
                    .default_width(520.0)
                    .show(ctx, |ui| {
                        close_requested |= Self::draw_popup_titlebar(ui, "Manage Plugins");
                        egui::Frame::NONE
                            .inner_margin(egui::Margin::same(12))
                            .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if ui.button("Open installed plugins folder").clicked() {
                                let dir = riverdeck_core::shared::config_dir().join("plugins");
                                #[cfg(target_os = "linux")]
                                {
                                    let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
                                }
                                #[cfg(target_os = "macos")]
                                {
                                    let _ = std::process::Command::new("open").arg(dir).spawn();
                                }
                                #[cfg(target_os = "windows")]
                                {
                                    let _ = std::process::Command::new("explorer").arg(dir).spawn();
                                }
                            }
                            if ui.button("Reload").clicked() {
                                // Invalidate caches that depend on plugin discovery.
                                self.plugins_cache = None;
                                self.plugins_inflight = false;
                                self.plugins_rx = None;
                                self.categories_cache = None;
                                self.categories_inflight = false;
                                self.categories_rx = None;
                                self.runtime.spawn(async {
                                    plugins::initialise_plugins();
                                });
                            }
                        });

                        ui.add_space(6.0);
                        if let Some(err) = self.plugin_manage_error.as_ref() {
                            ui.colored_label(ui.visuals().error_fg_color, err);
                        }
                        ui.checkbox(
                            &mut self.manage_plugins_show_action_uuids,
                            "Show action UUIDs (schema coverage)",
                        );
                        if self.manage_plugins_show_action_uuids {
                            // Ensure action/category cache is populated (normally driven by the Actions side panel).
                            if self.categories_cache.is_none()
                                && !self.categories_inflight
                                && self.categories_rx.is_none()
                            {
                                self.categories_inflight = true;
                                let (tx, rx) = mpsc::channel();
                                self.categories_rx = Some(rx);
                                self.runtime.spawn(async move {
                                    let _ = tx.send(riverdeck_core::api::get_categories().await);
                                });
                            }
                        }
                        if self.pending_plugin_install_result.is_some() {
                            ui.label(
                                egui::RichText::new("Installing…")
                                    .small()
                                    .color(ui.visuals().weak_text_color()),
                            );
                        }
                        if self.pending_plugin_remove_result.is_some() {
                            ui.label(
                                egui::RichText::new("Removing…")
                                    .small()
                                    .color(ui.visuals().weak_text_color()),
                            );
                        }
                        ui.separator();
                        if self.plugins_cache.is_none()
                            && !self.plugins_inflight
                            && self.plugins_rx.is_none()
                        {
                            self.plugins_inflight = true;
                            let (tx, rx) = mpsc::channel();
                            self.plugins_rx = Some(rx);
                            std::thread::spawn(move || {
                                let list = riverdeck_core::plugins::marketplace::list_local_plugins();
                                let _ = tx.send(list);
                            });
                        }
                        // Clone the display fields we need so we can call `texture_for_path` (needs `&mut self`)
                        // inside the ScrollArea closure without borrowing `self` immutably.
                        let plugins: Vec<LocalMarketplacePluginRow> = self
                            .plugins_cache
                            .as_ref()
                            .map(|v| {
                                v.iter()
                                    .map(|p| LocalMarketplacePluginRow {
                                        id: p.id.clone(),
                                        name: p.name.clone(),
                                        icon: p.icon.clone(),
                                        version: p.version.clone(),
                                        installed: p.installed,
                                        enabled: p.enabled,
                                        source: p.source,
                                        support: p.support.clone(),
                                        path: p.path.to_string_lossy().to_string(),
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();

                        egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                            if plugins.is_empty()
                                && (self.plugins_inflight || self.plugins_rx.is_some())
                            {
                                ui.label(
                                    egui::RichText::new("Loading plugins…")
                                        .small()
                                        .color(ui.visuals().weak_text_color()),
                                );
                            }
                            for LocalMarketplacePluginRow {
                                id,
                                name,
                                icon,
                                version,
                                installed,
                                enabled,
                                source,
                                support,
                                path,
                            } in plugins
                            {
                                egui::Frame::group(ui.style())
                                    .corner_radius(egui::CornerRadius::same(10))
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            let icon_size = egui::vec2(28.0, 28.0);
                                            if let Some(icon) = icon.as_ref()
                                                && let Some(tex) = self.texture_for_path(ctx, icon)
                                            {
                                                ui.image((tex.id(), icon_size));
                                            } else {
                                                ui.allocate_exact_size(icon_size, egui::Sense::hover());
                                            }

                                            ui.vertical(|ui| {
                                                ui.label(egui::RichText::new(&name).strong());
                                                ui.label(
                                                    egui::RichText::new(format!(
                                                        "{id}{}{}",
                                                        version
                                                            .as_ref()
                                                            .map(|v| format!(" • v{v}"))
                                                            .unwrap_or_default(),
                                                        if installed { " • installed" } else { " • available" },
                                                    ))
                                                        .small()
                                                        .color(ui.visuals().weak_text_color()),
                                                );
                                                let source_label = match source {
                                                    riverdeck_core::plugins::marketplace::PluginSource::Workspace => "workspace",
                                                    riverdeck_core::plugins::marketplace::PluginSource::Config => "config",
                                                };
                                                ui.label(
                                                    egui::RichText::new(format!("source: {source_label}"))
                                                        .small()
                                                        .color(ui.visuals().weak_text_color()),
                                                );
                                                match &support {
                                                    riverdeck_core::plugins::marketplace::PluginSupport::Supported => {}
                                                    riverdeck_core::plugins::marketplace::PluginSupport::Unsupported(reason) => {
                                                        ui.label(
                                                            egui::RichText::new("unsupported")
                                                                .small()
                                                                .color(ui.visuals().error_fg_color),
                                                        )
                                                        .on_hover_text(reason);
                                                    }
                                                }
                                            });

                                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                if installed {
                                                    if ui.button("Open").clicked() {
                                                        #[cfg(target_os = "linux")]
                                                        {
                                                            let _ = std::process::Command::new("xdg-open")
                                                                .arg(&path)
                                                                .spawn();
                                                        }
                                                        #[cfg(target_os = "macos")]
                                                        {
                                                            let _ = std::process::Command::new("open")
                                                                .arg(&path)
                                                                .spawn();
                                                        }
                                                        #[cfg(target_os = "windows")]
                                                        {
                                                            let _ = std::process::Command::new("explorer")
                                                                .arg(&path)
                                                                .spawn();
                                                        }
                                                    }
                                                    if ui
                                                        .add_enabled(
                                                            self.pending_plugin_install_result.is_none()
                                                                && self.pending_plugin_remove_result.is_none(),
                                                            egui::Button::new("Remove"),
                                                        )
                                                        .on_hover_text("Remove the installed plugin and delete its files")
                                                        .clicked()
                                                    {
                                                        self.pending_plugin_remove_confirm =
                                                            Some((id.clone(), name.clone()));
                                                    }
                                                } else if matches!(
                                                    source,
                                                    riverdeck_core::plugins::marketplace::PluginSource::Workspace
                                                ) && matches!(
                                                    support,
                                                    riverdeck_core::plugins::marketplace::PluginSupport::Supported
                                                ) && ui.button("Install").clicked()
                                                    && self.pending_plugin_install_result.is_none()
                                                    && self.pending_plugin_remove_result.is_none()
                                                {
                                                        let (tx, rx_done) = mpsc::channel();
                                                        self.pending_plugin_install_result = Some(rx_done);
                                                        let id = id.clone();
                                                        std::thread::spawn(move || {
                                                            let res = (|| -> anyhow::Result<()> {
                                                                riverdeck_core::plugins::marketplace::install_from_workspace(&id)?;
                                                                riverdeck_core::plugins::marketplace::set_plugin_enabled(&id, true)?;
                                                                Ok(())
                                                            })()
                                                            .map_err(|e| e.to_string());
                                                            let _ = tx.send(res);
                                                        });
                                                }

                                                let mut enabled_local = enabled;
                                                let toggle = ui.add_enabled(
                                                    self.pending_plugin_install_result.is_none()
                                                        && self.pending_plugin_remove_result.is_none(),
                                                    egui::Checkbox::new(&mut enabled_local, "Enabled"),
                                                );
                                                if toggle.changed() {
                                                    if let Err(err) = riverdeck_core::plugins::marketplace::set_plugin_enabled(&id, enabled_local) {
                                                        self.plugin_manage_error = Some(err.to_string());
                                                    } else {
                                                        self.plugin_manage_error = None;
                                                        // Invalidate caches that depend on plugin discovery.
                                                        self.plugins_cache = None;
                                                        self.plugins_inflight = false;
                                                        self.plugins_rx = None;
                                                        self.categories_cache = None;
                                                        self.categories_inflight = false;
                                                        self.categories_rx = None;
                                                        self.runtime.spawn(async {
                                                            plugins::initialise_plugins();
                                                        });
                                                    }
                                                }
                                            });
                                        });
                                    });
                                ui.add_space(6.0);
                            }
                        });

                        if self.manage_plugins_show_action_uuids {
                            ui.add_space(8.0);
                            ui.separator();
                            ui.label(
                                egui::RichText::new("Action UUIDs (schema coverage)")
                                    .strong(),
                            );

                            // Build a map plugin -> actions from the category cache.
                            let mut per_plugin: std::collections::BTreeMap<
                                String,
                                Vec<shared::Action>,
                            > = std::collections::BTreeMap::new();
                            for (_cat, cat) in self.categories_cache.clone().unwrap_or_default() {
                                for action in cat.actions {
                                    per_plugin
                                        .entry(action.plugin.clone())
                                        .or_default()
                                        .push(action);
                                }
                            }

                            // Plugin id -> plugin name for nicer headers.
                            let plugin_names: std::collections::HashMap<String, String> = self
                                .plugins_cache
                                .as_ref()
                                .map(|plugins| {
                                    plugins
                                        .iter()
                                        .map(|p| (p.id.clone(), p.name.clone()))
                                        .collect()
                                })
                                .unwrap_or_default();

                            egui::ScrollArea::vertical()
                                .max_height(260.0)
                                .show(ui, |ui| {
                                    if per_plugin.is_empty()
                                        && (self.categories_inflight
                                            || self.categories_rx.is_some())
                                    {
                                        ui.label(
                                            egui::RichText::new("Loading actions…")
                                                .small()
                                                .color(ui.visuals().weak_text_color()),
                                        );
                                    }
                                    for (plugin_id, mut actions) in per_plugin {
                                        actions.sort_by(|a, b| a.name.cmp(&b.name));
                                        let header = plugin_names
                                            .get(&plugin_id)
                                            .map(|n| format!("{n} ({plugin_id})"))
                                            .unwrap_or(plugin_id.clone());
                                        egui::CollapsingHeader::new(header)
                                            .default_open(false)
                                            .show(ui, |ui| {
                                                ui.spacing_mut().item_spacing.y = 6.0;
                                                for action in actions {
                                                    let has_pi = !action.property_inspector.trim().is_empty();
                                                    let has_schema = riverdeck_core::options_schema::get_schema(&action.uuid).is_some();
                                                    ui.horizontal(|ui| {
                                                        ui.label(egui::RichText::new(action.name).strong());
                                                        ui.label(
                                                            egui::RichText::new(action.uuid)
                                                                .small()
                                                                .color(ui.visuals().weak_text_color()),
                                                        );
                                                    });
                                                    ui.horizontal(|ui| {
                                                        ui.label(
                                                            egui::RichText::new(if has_pi { "PI" } else { "no PI" })
                                                                .small()
                                                                .color(if has_pi { ui.visuals().warn_fg_color } else { ui.visuals().weak_text_color() }),
                                                        );
                                                        ui.label(
                                                            egui::RichText::new(if has_schema { "schema" } else { "no schema" })
                                                                .small()
                                                                .color(if has_schema { ui.visuals().widgets.active.fg_stroke.color } else { ui.visuals().weak_text_color() }),
                                                        );
                                                    });
                                                    ui.separator();
                                                }
                                            });
                                    }
                                });
                        }

                        // Confirmation prompt for removal.
                        if let Some((remove_id, remove_name)) =
                            self.pending_plugin_remove_confirm.clone()
                        {
                            let mut open_confirm = true;
                            egui::Window::new("Remove plugin?")
                                .open(&mut open_confirm)
                                .collapsible(false)
                                .resizable(false)
                                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                                .show(ctx, |ui| {
                                    ui.label(egui::RichText::new(&remove_name).strong());
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{remove_id}\n\nThis will remove the plugin and delete its installed files.\nAll action instances from this plugin will be removed from your profiles."
                                        ))
                                        .small()
                                        .color(ui.visuals().weak_text_color()),
                                    );
                                    ui.add_space(8.0);
                                    ui.horizontal(|ui| {
                                        if ui.button("Cancel").clicked() {
                                            self.pending_plugin_remove_confirm = None;
                                        }
                                        if ui
                                            .add(
                                                egui::Button::new("Remove")
                                                    .fill(ui.visuals().error_fg_color),
                                            )
                                            .clicked()
                                            && self.pending_plugin_remove_result.is_none()
                                            && self.pending_plugin_install_result.is_none()
                                        {
                                            self.pending_plugin_remove_confirm = None;
                                            self.plugin_manage_error = None;
                                            let (tx, rx_done) = mpsc::channel();
                                            self.pending_plugin_remove_result = Some(rx_done);
                                            let id = remove_id.clone();
                                            self.runtime.spawn(async move {
                                                let res =
                                                    riverdeck_core::api::plugins::remove_plugin(id)
                                                        .await
                                                        .map_err(|e| e.to_string());
                                                let _ = tx.send(res);
                                            });
                                        }
                                    });
                                });
                            if !open_confirm {
                                self.pending_plugin_remove_confirm = None;
                            }
                        }
                            });
                    });
                if close_requested {
                    open = false;
                }
                if !open {
                    self.show_manage_plugins = false;
                }
            }

            if self.icon_library_open.is_some() {
                let mut window_open = true;
                let mut close_requested = false;
                let mut should_close = false;
                egui::Window::new("Icon Library")
                    .open(&mut window_open)
                    .title_bar(false)
                    .frame(
                        egui::Frame::window(ctx.style().as_ref())
                            .inner_margin(egui::Margin::ZERO),
                    )
                    .collapsible(false)
                    .resizable(true)
                    .default_width(740.0)
                    .default_height(520.0)
                    .show(ctx, |ui| {
                        close_requested |= Self::draw_popup_titlebar(ui, "Icon Library");
                        egui::Frame::NONE
                            .inner_margin(egui::Margin::same(12))
                            .show(ui, |ui| {
                                if self.icon_library_packs_cache.is_empty() {
                                    self.icon_library_packs_cache =
                                        riverdeck_core::api::icon_packs::list_icon_packs();
                                    if self.icon_library_selected_pack.is_none() {
                                        self.icon_library_selected_pack = self
                                            .icon_library_packs_cache
                                            .first()
                                            .map(|p| p.id.clone());
                                    }
                                }

                                ui.horizontal(|ui| {
                                    ui.label("Pack:");
                                    let selected = self
                                        .icon_library_selected_pack
                                        .clone()
                                        .unwrap_or_default();
                                    egui::ComboBox::from_id_salt("icon_library_pack")
                                        .selected_text(
                                            self.icon_library_packs_cache
                                                .iter()
                                                .find(|p| p.id == selected)
                                                .map(|p| p.name.as_str())
                                                .unwrap_or("<none>"),
                                        )
                                        .show_ui(ui, |ui| {
                                            for p in self.icon_library_packs_cache.iter() {
                                                if ui
                                                    .selectable_label(
                                                        Some(p.id.as_str())
                                                            == self.icon_library_selected_pack
                                                                .as_deref(),
                                                        format!(
                                                            "{}{}",
                                                            p.name,
                                                            if p.builtin { " (built-in)" } else { "" }
                                                        ),
                                                    )
                                                    .clicked()
                                                {
                                                    self.icon_library_selected_pack = Some(p.id.clone());
                                                    self.icon_library_cache_key = None;
                                                }
                                            }
                                        });

                                    ui.add_space(8.0);
                                    ui.label("Search:");
                                    let changed =
                                        ui.add(egui::TextEdit::singleline(&mut self.icon_library_query)).changed();
                                    if changed {
                                        self.icon_library_cache_key = None;
                                    }
                                });

                                let Some(pack_id) = self.icon_library_selected_pack.clone() else {
                                    ui.colored_label(
                                        ui.visuals().error_fg_color,
                                        "No icon packs installed.",
                                    );
                                    return;
                                };

                                let key = (pack_id.clone(), self.icon_library_query.clone());
                                if self.icon_library_cache_key.as_ref() != Some(&key) {
                                    match riverdeck_core::api::icon_packs::list_icons(
                                        &pack_id,
                                        Some(self.icon_library_query.as_str()),
                                    ) {
                                        Ok(icons) => {
                                            self.icon_library_icons_cache = icons;
                                            self.icon_library_cache_key = Some(key);
                                            self.icon_library_error = None;
                                        }
                                        Err(err) => {
                                            self.icon_library_icons_cache.clear();
                                            self.icon_library_cache_key = Some(key);
                                            self.icon_library_error = Some(err.to_string());
                                        }
                                    }
                                }

                                if let Some(err) = self.icon_library_error.as_ref() {
                                    ui.colored_label(ui.visuals().error_fg_color, err);
                                }

                                let Some((apply_ctx, apply_state)) =
                                    self.icon_library_open.clone()
                                else {
                                    return;
                                };

                                ui.add_space(6.0);
                                let icon_size = 54.0;
                                let spacing = 10.0;
                                let cols = ((ui.available_width() + spacing)
                                    / (icon_size + spacing))
                                    .floor()
                                    .max(1.0) as usize;

                                let mut clicked_icon: Option<String> = None;
                                egui::ScrollArea::vertical()
                                    .max_height(420.0)
                                    .show(ui, |ui| {
                                        if self.icon_library_icons_cache.is_empty() {
                                            ui.label(
                                                egui::RichText::new("No icons found.")
                                                    .small()
                                                    .color(ui.visuals().weak_text_color()),
                                            );
                                            return;
                                        }

                                        let mut i = 0usize;
                                        while i < self.icon_library_icons_cache.len() {
                                            ui.horizontal(|ui| {
                                                for _ in 0..cols {
                                                    if i >= self.icon_library_icons_cache.len() {
                                                        break;
                                                    }
                                                    // Avoid holding an immutable borrow of `self`
                                                    // across `texture_for_path` (which mutates the
                                                    // cache).
                                                    let (icon_path, icon_name) = {
                                                        let icon =
                                                            &self.icon_library_icons_cache[i];
                                                        (icon.path.clone(), icon.name.clone())
                                                    };

                                                    let mut resp = if let Some(tex) =
                                                        self.texture_for_path(ctx, &icon_path)
                                                    {
                                                        ui.add(
                                                            egui::ImageButton::new(
                                                                (
                                                                    tex.id(),
                                                                    egui::vec2(icon_size, icon_size),
                                                                ),
                                                            )
                                                            .corner_radius(
                                                                egui::CornerRadius::same(8),
                                                            ),
                                                        )
                                                    } else {
                                                        ui.add_sized(
                                                            [icon_size, icon_size],
                                                            egui::Button::new("…"),
                                                        )
                                                    };
                                                    if !icon_name.trim().is_empty() {
                                                        resp = resp.on_hover_text(&icon_name);
                                                    }
                                                    if resp.clicked() {
                                                        clicked_icon = Some(icon_path);
                                                    }
                                                    i += 1;
                                                }
                                            });
                                            ui.add_space(6.0);
                                        }
                                    });

                                if let Some(path) = clicked_icon {
                                    self.runtime.spawn(async move {
                                        let _ = riverdeck_core::api::instances::set_custom_icon_from_path(
                                            apply_ctx,
                                            Some(apply_state),
                                            path,
                                        )
                                        .await;
                                    });
                                    should_close = true;
                                }
                            });
                    });
                if close_requested || should_close {
                    window_open = false;
                }
                if !window_open {
                    self.icon_library_open = None;
                }
            }

            ui.add_space(12.0);

            let Some(snapshot) = &snapshot else {
                ui.label("Loading profile…");
                return;
            };

            // Preview-area click target: clicking truly empty space deselects the active slot.
            // (We intentionally keep this scoped to the preview area, not the whole center panel.)
            let (preview_rect, preview_resp) =
                ui.allocate_exact_size(ui.available_size(), egui::Sense::click());
            // Cache preview center so overlays can align with the preview, not with side panels.
            self.preview_center_x = Some(preview_rect.center().x);
            self.preview_width = Some(preview_rect.width());
            if preview_resp.clicked()
                && self.selected_slot.is_some()
                && self.drag_payload.is_none()
            {
                self.selected_slot = None;
                self.action_editor_open = false;
            }

            ui.allocate_new_ui(egui::UiBuilder::new().max_rect(preview_rect), |ui| {
                let key_size = egui::vec2(88.0, 88.0);
                let cols = device.columns as usize;
                let rows = device.rows as usize;
                // Center the keypad grid.
                let grid_spacing_x = ui.spacing().item_spacing.x;
                let grid_width = cols as f32 * key_size.x
                    + (cols.saturating_sub(1) as f32) * grid_spacing_x;
                let left_pad = ((ui.available_width() - grid_width) * 0.5).max(0.0);
                ui.horizontal(|ui| {
                    ui.add_space(left_pad);
                    ui.vertical(|ui| {
                        for r in 0..rows {
                            ui.horizontal(|ui| {
                                for c in 0..cols {
                                    let pos = (r * cols + c) as u8;
                                    let slot = SelectedSlot {
                                        controller: "Keypad".to_owned(),
                                        position: pos,
                                    };
                                    let instance = snapshot
                                        .keys
                                        .get(pos as usize)
                                        .and_then(|v| v.as_ref());
                                    let selected = self.selected_slot.as_ref() == Some(&slot);
                                    let resp = self.draw_slot_preview(
                                        ui,
                                        ctx,
                                        key_size,
                                        device,
                                        &slot,
                                        instance,
                                        selected,
                                        drag_payload.as_ref(),
                                    );
                                    resp.context_menu(|ui| {
                                        // Keep selection consistent with the slot being acted on.
                                        self.selected_slot = Some(slot.clone());
                                        self.action_editor_open = instance.is_some();

                                        let delete = ui.add_enabled(
                                            instance.is_some(),
                                            egui::Button::new("Delete"),
                                        );
                                        if delete.clicked() {
                                            ui.close_menu();
                                            let ctx_to_clear = shared::Context {
                                                device: device.id.clone(),
                                                profile: selected_profile.clone(),
                                                page: snapshot.page_id.clone(),
                                                controller: slot.controller.clone(),
                                                position: slot.position,
                                            };
                                            self.runtime.spawn(async move {
                                                let _ = riverdeck_core::api::instances::remove_instance(
                                                    shared::ActionContext::from_context(ctx_to_clear, 0),
                                                )
                                                .await;
                                            });
                                        }

                                        ui.separator();
                                        ui.add_enabled(false, egui::Button::new("More options…"));
                                    });
                                    if resp.clicked() {
                                        self.selected_slot = Some(slot.clone());
                                        self.action_editor_open = instance.is_some();
                                    }
                                    if resp.drag_started()
                                        && let Some(inst) = instance
                                    {
                                        let img = inst
                                            .states
                                            .get(inst.current_state as usize)
                                            .map(|s| s.image.trim())
                                            .filter(|s| !s.is_empty() && *s != "actionDefaultImage")
                                            .map(|s| s.to_owned())
                                            .unwrap_or_else(|| inst.action.icon.clone());
                                        self.drag_payload = Some(DragPayload::Slot {
                                            device_id: device.id.clone(),
                                            profile_id: selected_profile.clone(),
                                            page_id: snapshot.page_id.clone(),
                                            slot: slot.clone(),
                                            preview_name: inst.action.name.trim().to_owned(),
                                            preview_image_path: Some(img),
                                        });
                                    }
                                }
                            });
                        }
                    });
                });

                // Stream Deck+ style screen strip between keypad and encoders.
                self.draw_screen_strip(
                    ui,
                    ctx,
                    device,
                    snapshot,
                    &selected_profile,
                    left_pad,
                    grid_width,
                );

                if device.encoders > 0 {
                    // Tight spacing: encoders should sit closer to the strip/grid.
                    ui.add_space(4.0);
                    // Align encoders with the keypad grid start (same left padding).
                    ui.horizontal(|ui| {
                        ui.add_space(left_pad);
                        ui.horizontal_wrapped(|ui| {
                            for i in 0..(device.encoders as usize) {
                                let slot = SelectedSlot {
                                    controller: "Encoder".to_owned(),
                                    position: i as u8,
                                };
                                let instance = snapshot.sliders.get(i).and_then(|v| v.as_ref());
                                let selected = self.selected_slot.as_ref() == Some(&slot);
                                let resp = self.draw_encoder_dial_preview(
                                    ui,
                                    ctx,
                                    key_size,
                                    device,
                                    &slot,
                                    instance,
                                    selected,
                                    drag_payload.as_ref(),
                                );
                                resp.context_menu(|ui| {
                                    self.selected_slot = Some(slot.clone());
                                    self.action_editor_open = instance.is_some();

                                    let delete = ui.add_enabled(
                                        instance.is_some(),
                                        egui::Button::new("Delete"),
                                    );
                                    if delete.clicked() {
                                        ui.close_menu();
                                        let ctx_to_clear = shared::Context {
                                            device: device.id.clone(),
                                            profile: selected_profile.clone(),
                                            page: snapshot.page_id.clone(),
                                            controller: slot.controller.clone(),
                                            position: slot.position,
                                        };
                                        self.runtime.spawn(async move {
                                            let _ = riverdeck_core::api::instances::remove_instance(
                                                shared::ActionContext::from_context(ctx_to_clear, 0),
                                            )
                                            .await;
                                        });
                                    }

                                    ui.separator();
                                    ui.add_enabled(false, egui::Button::new("More options…"));
                                });
                                if resp.clicked() {
                                    self.selected_slot = Some(slot.clone());
                                    self.action_editor_open = instance.is_some();
                                }
                                if resp.drag_started()
                                    && let Some(inst) = instance
                                {
                                    let img = inst
                                        .states
                                        .get(inst.current_state as usize)
                                        .map(|s| s.image.trim())
                                        .filter(|s| !s.is_empty() && *s != "actionDefaultImage")
                                        .map(|s| s.to_owned())
                                        .unwrap_or_else(|| inst.action.icon.clone());
                                    self.drag_payload = Some(DragPayload::Slot {
                                        device_id: device.id.clone(),
                                        profile_id: selected_profile.clone(),
                                        page_id: snapshot.page_id.clone(),
                                        slot: slot.clone(),
                                        preview_name: inst.action.name.trim().to_owned(),
                                        preview_image_path: Some(img),
                                    });
                                }
                            }
                        });
                    });
                }

                // Pages (real pages inside the selected profile).
                ui.add_space(10.0);
                let pages_key = (device.id.clone(), selected_profile.clone());
                if !self.pages_cache.contains_key(&pages_key)
                    && self.pages_inflight.as_ref() != Some(&pages_key)
                    && self.pages_rx.is_none()
                {
                    self.pages_inflight = Some(pages_key.clone());
                    let (tx, rx) = mpsc::channel();
                    self.pages_rx = Some(rx);
                    let (device_id, profile_id) = pages_key.clone();
                    self.runtime.spawn(async move {
                        let pages = riverdeck_core::api::pages::get_pages(
                            device_id.clone(),
                            profile_id.clone(),
                        )
                        .await
                        .unwrap_or_else(|_| vec!["1".to_owned()]);
                        let _ = tx.send(((device_id, profile_id), pages));
                    });
                }
                let pages = self
                    .pages_cache
                    .get(&pages_key)
                    .cloned()
                    .unwrap_or_else(|| vec!["1".to_owned()]);
                let selected_page = snapshot.page_id.clone();

                ui.horizontal(|ui| {
                    // Center the page bar under the grid by reusing the same padding.
                    ui.add_space(left_pad);
                    ui.horizontal(|ui| {
                        ui.label("Pages:");

                        for page_id in pages.iter() {
                            let label = if let Ok(n) = page_id.parse::<u32>() {
                                n.to_string()
                            } else {
                                page_id.clone()
                            };

                            let selected = page_id == &selected_page;
                            let resp = ui.selectable_label(selected, label);

                            if resp.clicked() {
                                let device_id = device.id.clone();
                                let profile_id = selected_profile.clone();
                                let page_id = page_id.clone();
                                self.runtime.spawn(async move {
                                    let _ = riverdeck_core::api::pages::set_selected_page(
                                        device_id,
                                        profile_id,
                                        page_id,
                                    )
                                    .await;
                                });
                            }

                            // Right-click delete page (keep at least one page).
                            if pages.len() > 1 {
                                resp.context_menu(|ui| {
                                    if ui.button("Delete page").clicked() {
                                        ui.close_menu();
                                        // Invalidate cached page list; it will be refetched asynchronously.
                                        self.pages_cache.remove(&pages_key);
                                        let device_id = device.id.clone();
                                        let profile_id = selected_profile.clone();
                                        let page_id = page_id.clone();
                                        self.runtime.spawn(async move {
                                            let _ = riverdeck_core::api::pages::delete_page(
                                                device_id,
                                                profile_id,
                                                page_id,
                                            )
                                            .await;
                                        });
                                    }
                                });
                            }
                        }

                        if ui.button("+").clicked() {
                            // Invalidate cached page list; it will be refetched asynchronously.
                            self.pages_cache.remove(&pages_key);
                            let device_id = device.id.clone();
                            let profile_id = selected_profile.clone();
                            self.runtime.spawn(async move {
                                let _ = riverdeck_core::api::pages::create_page_and_select(
                                    device_id,
                                    profile_id,
                                )
                                .await;
                            });
                        }
                    });
                });
            });
        });

        // Floating drag preview + drop handling.
        if let Some(payload) = self.drag_payload.clone() {
            if let Some(pos) = ctx.input(|i| i.pointer.latest_pos()) {
                let preview_pos = pos + egui::vec2(16.0, 16.0);
                egui::Area::new("riverdeck_drag_preview".into())
                    .fixed_pos(preview_pos)
                    .interactable(false)
                    .show(ctx, |ui| {
                        egui::Frame::popup(ui.style())
                            .corner_radius(egui::CornerRadius::same(10))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    let icon_size = egui::vec2(32.0, 32.0);
                                    match &payload {
                                        DragPayload::Action(action_preview) => {
                                            if let Some(path) =
                                                Self::action_icon_path(action_preview)
                                                && let Some(tex) = self.texture_for_path(ctx, path)
                                            {
                                                ui.image((tex.id(), icon_size));
                                            }
                                            ui.label(&action_preview.name);
                                        }
                                        DragPayload::Slot {
                                            preview_name,
                                            preview_image_path,
                                            ..
                                        } => {
                                            if let Some(path) =
                                                preview_image_path.as_ref().map(|s| s.as_str())
                                                && let Some(tex) = self.texture_for_path(ctx, path)
                                            {
                                                ui.image((tex.id(), icon_size));
                                            }
                                            ui.label(preview_name);
                                        }
                                    }
                                });
                            });
                    });
            }

            // On mouse/touch release: apply if hovering a valid slot, otherwise cancel.
            if ctx.input(|i| i.pointer.any_released()) {
                if self.drag_hover_valid
                    && let Some(slot) = self.drag_hover_slot.clone()
                    && let Some(device) = selected_device.as_ref()
                {
                    let page_id = snapshot
                        .as_ref()
                        .map(|s| s.page_id.clone())
                        .unwrap_or_else(|| "1".to_owned());

                    match payload {
                        DragPayload::Action(action) => {
                            let ctx_to_set = shared::Context {
                                device: device.id.clone(),
                                profile: selected_profile.clone(),
                                page: page_id,
                                controller: slot.controller.clone(),
                                position: slot.position,
                            };
                            let _ = self.runtime.block_on(async {
                                riverdeck_core::api::instances::create_instance(action, ctx_to_set)
                                    .await
                            });
                        }
                        DragPayload::Slot {
                            device_id,
                            profile_id,
                            page_id: drag_page_id,
                            slot: source_slot,
                            ..
                        } => {
                            // Only allow within the same current device/profile/page to avoid
                            // surprising cross-page operations if the UI changes during drag.
                            if device_id == device.id
                                && profile_id == selected_profile
                                && drag_page_id == page_id
                            {
                                let src = shared::Context {
                                    device: device_id,
                                    profile: profile_id.clone(),
                                    page: drag_page_id.clone(),
                                    controller: source_slot.controller.clone(),
                                    position: source_slot.position,
                                };
                                let dst = shared::Context {
                                    device: device.id.clone(),
                                    profile: profile_id,
                                    page: drag_page_id,
                                    controller: slot.controller.clone(),
                                    position: slot.position,
                                };
                                self.runtime.spawn(async move {
                                    let _ =
                                        riverdeck_core::api::instances::swap_instances(src, dst)
                                            .await;
                                });
                            }
                        }
                    }
                }

                self.drag_payload = None;
                self.drag_hover_slot = None;
                self.drag_hover_valid = false;
            }
        }

        // Avoid unconditional 60 FPS redraws; only repaint aggressively when needed.
        // (Dragging, pending async refresh, or active preview updates should request repaints explicitly.)
        if self.drag_payload.is_some() {
            ctx.request_repaint();
        }

        // Bottom-right toast notifications (plugin installs, etc).
        self.draw_toasts(ctx);
    }
}

impl Drop for RiverDeckApp {
    fn drop(&mut self) {
        self.close_marketplace();
        self.close_property_inspector();
        // Ensure we don't orphan plugin subprocesses (native/node/wine) on window close or tray quit.
        // This is synchronous so it runs even if the UI is exiting.
        self.runtime
            .block_on(async { riverdeck_core::lifecycle::shutdown_all().await });
    }
}

#[cfg(target_os = "linux")]
impl RiverDeckApp {
    fn draw_custom_titlebar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // A simple custom title bar so we can always render window controls on the top-right.
        let height = 28.0;
        let (_size, resp) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), height),
            egui::Sense::click_and_drag(),
        );
        let rect = resp.rect;

        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(rect), |ui| {
            // Split into three clipped regions so the title can be centered reliably.
            let outer_pad = 6.0;
            let buttons_w = 96.0;
            let gap = 6.0;

            let right_x1 = rect.right() - outer_pad;
            let right_x0 = right_x1 - buttons_w;
            let available_for_title = (right_x0 - gap) - rect.left();
            let title_w = 200.0f32.min(available_for_title.max(60.0));
            let title_x0 = (rect.center().x - title_w / 2.0).max(rect.left());
            let title_x1 = (title_x0 + title_w).min(right_x0 - gap);

            let left_rect = egui::Rect::from_min_max(
                rect.left_top(),
                egui::pos2((title_x0 - gap).max(rect.left()), rect.bottom()),
            );
            let title_rect = egui::Rect::from_min_max(
                egui::pos2(title_x0, rect.top()),
                egui::pos2(title_x1, rect.bottom()),
            );
            let right_rect = egui::Rect::from_min_max(
                egui::pos2(right_x0, rect.top()),
                egui::pos2(right_x1, rect.bottom()),
            );

            // Left: status/info.
            ui.allocate_new_ui(egui::UiBuilder::new().max_rect(left_rect), |ui| {
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if let Some(tex) = self.embedded_logo_texture(ctx) {
                        ui.image((tex.id(), egui::vec2(20.0, 20.0)));
                    }
                    ui.label(format!("devices: {}", shared::DEVICES.len()));

                    if let Some(info) = self.update_info.lock().unwrap().as_ref() {
                        ui.separator();
                        ui.label(format!("Update available: {}", info.tag));
                        if ui.button("Details").clicked() {
                            self.show_update_details = true;
                        }
                    }
                });
            });

            // Center: window title (drag handle).
            ui.allocate_new_ui(egui::UiBuilder::new().max_rect(title_rect), |ui| {
                ui.with_layout(
                    egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                    |ui| {
                        // Only this label acts as the drag handle, so clicks on the window buttons work reliably.
                        // Keep the settings icon visually close to the title.
                        ui.spacing_mut().item_spacing.x = 2.0;
                        ui.spacing_mut().button_padding = egui::vec2(2.0, 0.0);
                        let drag = ui.add(
                            egui::Label::new(egui::RichText::new("RiverDeck").strong())
                                .sense(egui::Sense::click_and_drag()),
                        );
                        if drag.drag_started() || drag.dragged() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                        }

                        let cog = ui
                            .add(
                                egui::Button::new(egui::RichText::new("⚙").size(14.0)).frame(false),
                            )
                            .on_hover_text("Settings");
                        if cog.clicked() {
                            self.show_settings = true;
                            self.settings_error = None;
                            // Load current settings into the local UI state (best effort).
                            match riverdeck_core::store::get_settings() {
                                Ok(store) => {
                                    self.settings_autostart = store.value.autolaunch;
                                    self.settings_screensaver = store.value.screensaver;
                                }
                                Err(err) => {
                                    self.settings_error = Some(err.to_string());
                                }
                            }
                        }
                    },
                );
            });

            // Right: window buttons.
            ui.allocate_new_ui(egui::UiBuilder::new().max_rect(right_rect), |ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    ui.spacing_mut().button_padding = egui::vec2(7.0, 3.0);
                    let btn_size = egui::vec2(26.0, 20.0);

                    // Use plain ASCII so it renders reliably even when the active font lacks the ✕ glyph.
                    let close = ui.add(egui::Button::new("X").min_size(btn_size));
                    if close.clicked() {
                        // Always request a real close; the `close_requested` handler above will
                        // decide whether to hide-to-tray (only when explicitly enabled).
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }

                    let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                    let max_label = if maximized { "❐" } else { "▢" };
                    if ui
                        .add(egui::Button::new(max_label).min_size(btn_size))
                        .clicked()
                    {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                    }

                    if ui.add(egui::Button::new("—").min_size(btn_size)).clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                    }
                });
            });
        });

        // Make the rest of the titlebar (the empty space) draggable too.
        // Widgets (buttons/labels) added above will win pointer interactions over this background,
        // so this mainly triggers when dragging on "unused" titlebar space.
        if resp.drag_started() || resp.dragged() {
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
    }
}

#[cfg(feature = "tray")]
fn log_tray_status_to_file(msg: &str) {
    // Best-effort. Ignore all errors.
    let path = shared::log_dir().join("riverdeck-egui-tray.log");
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!("[{ts}] {msg}\n");
    let _ = std::fs::create_dir_all(shared::log_dir());
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

#[derive(Clone)]
struct UpdateInfo {
    tag: String,
    body: String,
}

fn start_update_check(runtime: Arc<Runtime>, update_info: Arc<Mutex<Option<UpdateInfo>>>) {
    let current_version = semver::Version::parse(env!("CARGO_PKG_VERSION")).ok();
    runtime.spawn(async move {
        // Respect stored setting if present.
        if let Ok(store) = riverdeck_core::store::get_settings()
            && !store.value.updatecheck
        {
            return;
        }

        let res = reqwest::Client::new()
            .get("https://api.github.com/repos/sulrwin/RiverDeck/releases/latest")
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "RiverDeck")
            .send()
            .await;
        let Ok(res) = res else { return };
        let Ok(json) = res.json::<serde_json::Value>().await else {
            return;
        };

        let tag_name = json.get("tag_name").and_then(|v| v.as_str()).unwrap_or("");
        let body = json
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_owned();
        let remote = tag_name.strip_prefix('v').unwrap_or(tag_name);

        if let (Some(cur), Ok(remote)) = (current_version, semver::Version::parse(remote))
            && cur < remote
        {
            *update_info.lock().unwrap() = Some(UpdateInfo {
                tag: tag_name.to_owned(),
                body,
            });
        }
    });
}

#[cfg(feature = "tray")]
struct TrayState {
    #[allow(dead_code)]
    _tray: TrayIcon,
    show: MenuItem,
    hide: MenuItem,
    quit: MenuItem,
}

#[cfg(feature = "tray")]
impl TrayState {
    fn new() -> anyhow::Result<Self> {
        // `tray-icon` on Linux uses GTK menus and will panic if GTK is not initialized.
        // We initialize it here (best-effort) and bubble errors so the caller can
        // disable tray support gracefully.
        #[cfg(target_os = "linux")]
        {
            gtk::init().map_err(|e| anyhow::anyhow!("failed to init GTK: {e}"))?;
        }

        let menu = Menu::new();
        let show = MenuItem::new("Show", true, None);
        let hide = MenuItem::new("Hide", true, None);
        let quit = MenuItem::new("Quit", true, None);
        let separator = PredefinedMenuItem::separator();
        menu.append(&show)?;
        menu.append(&hide)?;
        menu.append(&separator)?;
        menu.append(&quit)?;

        let icon = load_tray_icon().ok();
        let tray = {
            let mut b = TrayIconBuilder::new().with_menu(Box::new(menu));
            if let Some(icon) = icon {
                b = b.with_icon(icon);
            }
            b.with_tooltip("RiverDeck").build()?
        };

        Ok(Self {
            _tray: tray,
            show,
            hide,
            quit,
        })
    }

    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == self.show.id() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            } else if ev.id == self.hide.id() {
                // Hide to tray (no taskbar entry).
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            } else if ev.id == self.quit.id() {
                QUIT_REQUESTED.store(true, Ordering::SeqCst);
                // Make sure the close isn't intercepted by hide-to-tray logic.
                // We rely on the normal shutdown path (signal handlers / Drop) to clean up.
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
}

#[cfg(feature = "tray")]
fn load_tray_icon() -> anyhow::Result<Icon> {
    let (rgba, width, height) = load_embedded_logo_rgba()?;
    Ok(Icon::from_rgba(rgba, width, height)?)
}

fn spawn_web_process(
    label: &str,
    url: &str,
    dock: Option<(i32, i32, i32, i32)>,
) -> anyhow::Result<std::process::Child> {
    let mut cmd = std::process::Command::new(resolve_pi_exe()?);
    configure_pi_webview_env(&mut cmd);
    cmd.arg("--label").arg(label).arg("--url").arg(url);
    cmd.arg("--storage-dir").arg(
        riverdeck_core::shared::data_dir()
            .join("webview")
            .join("web"),
    );
    if let Some(port) = IPC_PORT.get() {
        cmd.arg("--ipc-port").arg(port.to_string());
    }

    if let Some((x, y, w, h)) = dock {
        cmd.arg("--x")
            .arg(x.to_string())
            .arg("--y")
            .arg(y.to_string())
            .arg("--w")
            .arg(w.to_string())
            .arg("--h")
            .arg(h.to_string())
            .arg("--decorations")
            .arg("1");
    }

    let pi_log_dir = riverdeck_core::shared::log_dir().join("pi");
    let _ = std::fs::create_dir_all(&pi_log_dir);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let log_file_path = pi_log_dir.join(format!("riverdeck-pi-web-{ts}.log"));
    let child = spawn_child_with_captured_pi_logs(cmd, &log_file_path)?;
    riverdeck_core::runtime_processes::record_process(
        child.id(),
        "riverdeck_pi_web",
        vec!["--label".to_owned(), label.to_owned(), "--url".to_owned()],
    );
    Ok(child)
}

#[allow(clippy::too_many_arguments)]
fn spawn_pi_process(
    label: &str,
    pi_src: &str,
    origin: &str,
    ws_port: u16,
    context: &str,
    info_json: &str,
    connect_json: &str,
    dock: Option<(i32, i32, i32, i32)>,
) -> anyhow::Result<std::process::Child> {
    let mut cmd = std::process::Command::new(resolve_pi_exe()?);
    configure_pi_webview_env(&mut cmd);
    cmd.arg("--label")
        .arg(label)
        .arg("--pi-src")
        .arg(pi_src)
        .arg("--origin")
        .arg(origin)
        .arg("--ws-port")
        .arg(ws_port.to_string())
        .arg("--context")
        .arg(context)
        .arg("--info-json")
        .arg(info_json)
        .arg("--connect-json")
        .arg(connect_json);
    cmd.arg("--storage-dir").arg(
        riverdeck_core::shared::data_dir()
            .join("webview")
            .join("pi"),
    );
    if let Some(port) = IPC_PORT.get() {
        cmd.arg("--ipc-port").arg(port.to_string());
    }

    if let Some((x, y, w, h)) = dock {
        cmd.arg("--x")
            .arg(x.to_string())
            .arg("--y")
            .arg(y.to_string())
            .arg("--w")
            .arg(w.to_string())
            .arg("--h")
            .arg(h.to_string())
            .arg("--decorations")
            .arg("1");
    }

    let pi_log_dir = riverdeck_core::shared::log_dir().join("pi");
    let _ = std::fs::create_dir_all(&pi_log_dir);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let log_file_path = pi_log_dir.join(format!("riverdeck-pi-pi-{ts}.log"));
    let child = spawn_child_with_captured_pi_logs(cmd, &log_file_path)?;
    riverdeck_core::runtime_processes::record_process(
        child.id(),
        "riverdeck_pi_pi",
        vec![
            "--label".to_owned(),
            label.to_owned(),
            "--pi-src".to_owned(),
            pi_src.to_owned(),
        ],
    );
    Ok(child)
}

fn spawn_child_with_captured_pi_logs(
    mut cmd: std::process::Command,
    log_file_path: &std::path::Path,
) -> anyhow::Result<std::process::Child> {
    // If we can't create the log file, fall back to inheriting the parent's stdio.
    let log_file = match std::fs::File::create(log_file_path) {
        Ok(f) => f,
        Err(_) => return Ok(cmd.spawn()?),
    };

    // We want to keep PI helper logs, but `tao`'s GTK backend emits a debug-only `eprintln!`
    // spam line for unmapped keycodes:
    //   "Couldn't get key from code: Unidentified(Gtk(248))"
    // That line is not actionable for most users and drowns out real issues.
    //
    // Opt back in via `RIVERDECK_PI_LOG_UNKNOWN_KEYS=1` (or `RIVERDECK_WEBVIEW_LOG_UNKNOWN_KEYS=1`).
    let log_unknown_keys = env_truthy_any("RIVERDECK_PI_LOG_UNKNOWN_KEYS")
        || env_truthy_any("RIVERDECK_WEBVIEW_LOG_UNKNOWN_KEYS");

    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn()?;
    let file = std::sync::Arc::new(std::sync::Mutex::new(log_file));

    // Pump stdout/stderr into the same file (best-effort). Use a mutex to avoid interleaving writes.
    if let Some(stdout) = child.stdout.take() {
        spawn_pipe_pump_thread(stdout, std::sync::Arc::clone(&file), log_unknown_keys);
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_pipe_pump_thread(stderr, std::sync::Arc::clone(&file), log_unknown_keys);
    }

    Ok(child)
}

fn spawn_pipe_pump_thread<R>(
    reader: R,
    file: std::sync::Arc<std::sync::Mutex<std::fs::File>>,
    log_unknown_keys: bool,
) where
    R: std::io::Read + Send + 'static,
{
    std::thread::spawn(move || {
        use std::io::{BufRead as _, Write as _};

        const TAO_UNKNOWN_KEY_PREFIX: &[u8] = b"Couldn't get key from code:";

        let mut r = std::io::BufReader::new(reader);
        let mut buf = Vec::<u8>::with_capacity(4096);
        loop {
            buf.clear();
            match r.read_until(b'\n', &mut buf) {
                Ok(0) => break,
                Ok(_) => {
                    if !log_unknown_keys && buf.starts_with(TAO_UNKNOWN_KEY_PREFIX) {
                        continue;
                    }
                    if let Ok(mut f) = file.lock() {
                        let _ = f.write_all(&buf);
                    }
                }
                Err(_) => break,
            }
        }
    });
}

fn configure_pi_webview_env(cmd: &mut std::process::Command) {
    // On some Wayland setups, wry/webkit can fail at runtime with:
    // "Error: the window handle kind is not supported".
    // Work around by forcing the webview helper onto X11 (XWayland) for now.
    #[cfg(target_os = "linux")]
    {
        // Make helper logs visible even when RiverDeck itself is running with a restricted
        // `RUST_LOG` filter. We append our module filter instead of replacing.
        let mut rust_log = std::env::var("RUST_LOG").unwrap_or_default();
        if !rust_log.contains("riverdeck_pi=") {
            if !rust_log.trim().is_empty() && !rust_log.ends_with(',') {
                rust_log.push(',');
            }
            rust_log.push_str("riverdeck_pi=debug");
        }
        cmd.env("RUST_LOG", rust_log);
        cmd.env("RUST_BACKTRACE", "1");

        let session = std::env::var("XDG_SESSION_TYPE")
            .unwrap_or_default()
            .to_lowercase();
        if session == "wayland" && std::env::var("DISPLAY").ok().is_some() {
            // Default: force X11 (XWayland). This is the most reliable mode for `wry`/WebKitGTK
            // across distros/compositors today.
            let prefer_wayland = env_truthy_any("RIVERDECK_PI_PREFER_WAYLAND")
                || env_truthy_any("RIVERDECK_WEBVIEW_PREFER_WAYLAND");
            let force_x11 = env_truthy_any("RIVERDECK_PI_FORCE_X11")
                || env_truthy_any("RIVERDECK_WEBVIEW_FORCE_X11");

            if force_x11 || !prefer_wayland {
                cmd.env("GDK_BACKEND", "x11");
                cmd.env("WINIT_UNIX_BACKEND", "x11");
            }
        }

        // Further hardening: WebKitGTK can show a white window when GPU-backed compositing/DMABuf
        // paths misbehave (seen on both X11 and XWayland depending on driver/compositor).
        // Prefer reliability for our small helper webviews.
        //
        // Opt out by setting `RIVERDECK_PI_ALLOW_GPU=1` (or `RIVERDECK_WEBVIEW_ALLOW_GPU=1`).
        let allow_gpu = env_truthy_any("RIVERDECK_PI_ALLOW_GPU")
            || env_truthy_any("RIVERDECK_WEBVIEW_ALLOW_GPU");
        if !allow_gpu {
            cmd.env("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
            cmd.env("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
            cmd.env("WEBKIT_DISABLE_GPU_PROCESS", "1");
            cmd.env("LIBGL_ALWAYS_SOFTWARE", "1");

            // NVIDIA (and some compositor setups) can glitch with explicit sync enabled.
            // Best-effort: disable it for the helper process only.
            cmd.env("__NV_DISABLE_EXPLICIT_SYNC", "1");
        }
    }
}

fn pi_exe_basename() -> &'static str {
    if cfg!(windows) {
        "riverdeck-pi.exe"
    } else {
        "riverdeck-pi"
    }
}

fn resolve_pi_exe() -> anyhow::Result<std::ffi::OsString> {
    #[cfg(debug_assertions)]
    fn dev_pi_needs_build(repo_root: &std::path::Path, candidate: &std::path::Path) -> bool {
        if !candidate.exists() {
            return true;
        }
        let Ok(bin_meta) = std::fs::metadata(candidate) else {
            return true;
        };
        let Ok(bin_m) = bin_meta.modified() else {
            return true;
        };
        let src = repo_root
            .join("crates")
            .join("riverdeck-pi")
            .join("src")
            .join("main.rs");
        let Ok(src_meta) = std::fs::metadata(src) else {
            return false;
        };
        let Ok(src_m) = src_meta.modified() else {
            return false;
        };
        src_m > bin_m
    }

    // 1) Prefer a sibling binary next to the current executable (packaged installs).
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join(pi_exe_basename());
        if candidate.exists() {
            #[cfg(debug_assertions)]
            {
                // In dev, the "sibling binary" is usually `target/debug/riverdeck-pi`. Keep it fresh.
                if let Ok(cwd) = std::env::current_dir()
                    && cwd.join("Cargo.toml").is_file()
                    && dev_pi_needs_build(&cwd, &candidate)
                {
                    let _ = std::process::Command::new("cargo")
                        .args(["build", "-p", "riverdeck-pi"])
                        .status();
                }
            }
            log::debug!("Using webview helper: {}", candidate.to_string_lossy());
            return Ok(candidate.into_os_string());
        }
    }

    // 2) Dev convenience: prefer the workspace `target/<profile>/riverdeck-pi` when running from
    // a checkout. `cargo run -p riverdeck-egui` does not necessarily build `riverdeck-pi`, so we
    // optionally build it on-demand (best-effort) to keep the in-app marketplace working.
    #[cfg(debug_assertions)]
    {
        let cwd = std::env::current_dir().ok();
        if let Some(cwd) = cwd {
            let debug_candidate = cwd.join("target").join("debug").join(pi_exe_basename());
            let needs_build = (|| {
                if !debug_candidate.exists() {
                    return true;
                }
                // Rebuild if sources look newer than the binary (common when iterating on `riverdeck-pi`
                // while running `cargo run -p riverdeck-egui`).
                let Ok(bin_meta) = std::fs::metadata(&debug_candidate) else {
                    return true;
                };
                let Ok(bin_m) = bin_meta.modified() else {
                    return true;
                };
                let src_root = cwd.join("crates").join("riverdeck-pi").join("src");
                let src = src_root.join("main.rs");
                let Ok(src_meta) = std::fs::metadata(src) else {
                    return false;
                };
                let Ok(src_m) = src_meta.modified() else {
                    return false;
                };
                src_m > bin_m
            })();
            if !needs_build && debug_candidate.exists() {
                log::debug!(
                    "Using webview helper: {}",
                    debug_candidate.to_string_lossy()
                );
                return Ok(debug_candidate.into_os_string());
            }
            // If this looks like the repo root, try to build `riverdeck-pi` once.
            if cwd.join("Cargo.toml").is_file() {
                let status = std::process::Command::new("cargo")
                    .args(["build", "-p", "riverdeck-pi"])
                    .status();
                if status.map(|s| s.success()).unwrap_or(false) && debug_candidate.exists() {
                    log::debug!(
                        "Using webview helper: {}",
                        debug_candidate.to_string_lossy()
                    );
                    return Ok(debug_candidate.into_os_string());
                }
            }
        }
    }

    // 3) Fall back to PATH.
    log::debug!("Using webview helper from PATH: {}", pi_exe_basename());
    Ok(std::ffi::OsString::from(pi_exe_basename()))
}

fn is_process_not_found(err: &anyhow::Error) -> bool {
    err.downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}
