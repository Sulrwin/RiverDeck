use riverdeck_core::{application_watcher, elgato, plugins, shared, ui};

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;
use std::sync::mpsc;
use std::{
    fs,
    fs::OpenOptions,
    path::{Path, PathBuf},
    sync::Mutex,
};

use fs2::FileExt;
use tokio::runtime::Runtime;
use tokio::sync::broadcast;

#[cfg(feature = "tray")]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(feature = "tray")]
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
#[cfg(feature = "tray")]
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

#[cfg(feature = "tray")]
static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

fn main() -> anyhow::Result<()> {
    // Default to info logging unless the user overrides with RUST_LOG.
    // (When started from a desktop entry, stdout/stderr may not be visible, but
    // this still helps for terminal/Cursor runs.)
    {
        use env_logger::Env;
        let _ = env_logger::Builder::from_env(Env::default().default_filter_or("info")).try_init();
    }

    // Development quality-of-life: when running under an IDE/debugger, it's easy to end up with an
    // orphaned `riverdeck` process if the parent launcher is force-killed.
    //
    // On Linux in debug builds, request SIGTERM when our parent dies.
    #[cfg(target_os = "linux")]
    set_parent_death_signal();

    let args: Vec<String> = std::env::args().collect();
    let start_hidden = args.iter().any(|a| a == "--hide");
    let replace_instance = args.iter().any(|a| a == "--replace");

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
        } else if looks_like_bundled_resources(exe_dir) {
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

    configure_autostart();

    // Single-instance: lockfile + PID. In dev/debug, Cursor/VScode sometimes kills the `cargo`
    // parent but leaves the spawned `riverdeck` binary running, so we support a best-effort
    // takeover (`--replace`) to make restarts reliable.
    std::fs::create_dir_all(shared::config_dir())?;
    let mut lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(shared::config_dir().join("riverdeck.lock"))?;
    if lock_file.try_lock_exclusive().is_err() {
        // Another instance is running.
        let should_replace = replace_instance
            || cfg!(debug_assertions)
            || std::env::var("RIVERDECK_REPLACE_INSTANCE").ok().as_deref() == Some("1");

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
            log::warn!("RiverDeck already running (lockfile held). Pass `--replace` to take over.");
            return Ok(());
        }
    }

    // Record our PID into the lock file (best-effort).
    let _ = write_lock_pid(&mut lock_file);

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
    start_signal_handlers(runtime.clone());
    start_core_background(runtime.clone());

    let update_info: Arc<Mutex<Option<UpdateInfo>>> = Arc::new(Mutex::new(None));
    start_update_check(runtime.clone(), update_info.clone());
    handle_startup_args(runtime.clone(), args.clone());

    // Exit the process when the main window is closed.
    // This prevents "background instances" that keep running after the window is gone.
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
    if let Ok(icon) = load_window_icon() {
        native_options.viewport = native_options.viewport.with_icon(icon);
    }
    eframe::run_native(
        "RiverDeck",
        native_options,
        Box::new(move |_cc| {
            Ok(Box::new(RiverDeckApp::new(
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

        tokio::select! {
            _ = sigterm.recv() => {},
            _ = sigint.recv() => {},
        }

        log::warn!("Received termination signal; shutting down RiverDeck");
        riverdeck_core::lifecycle::shutdown_all().await;
        std::process::exit(0);
    });
}

#[cfg(not(unix))]
fn start_signal_handlers(_runtime: Arc<Runtime>) {}

fn load_window_icon() -> anyhow::Result<egui::IconData> {
    let (rgba, width, height) = load_embedded_logo_rgba()?;
    Ok(egui::IconData {
        rgba,
        width,
        height,
    })
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

fn write_lock_pid(lock_file: &mut std::fs::File) -> std::io::Result<()> {
    lock_file.set_len(0)?;
    lock_file.seek(SeekFrom::Start(0))?;
    writeln!(lock_file, "{}", std::process::id())?;
    lock_file.sync_all()?;
    Ok(())
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

#[cfg(all(unix, target_os = "linux"))]
fn linux_pid_looks_like_riverdeck(pid: u32) -> bool {
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok();
    let Some(exe) = exe else {
        return false;
    };
    let Some(name) = exe.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
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
                || arg.starts_with("streamdeck://")
                || arg.starts_with("riverdeck://")
            {
                if let Ok(url) = reqwest::Url::parse(arg)
                    && url.host_str() == Some("plugins")
                    && let Some(mut path) = url.path_segments()
                    && path.next() == Some("message")
                    && let Some(plugin_id) = path.next()
                {
                    let plugin_id = if url.scheme() == "streamdeck" {
                        format!("{plugin_id}.sdPlugin")
                    } else {
                        plugin_id.to_owned()
                    };
                    let _ = riverdeck_core::events::outbound::deep_link::did_receive_deep_link(
                        &plugin_id,
                        arg.clone(),
                    )
                    .await;
                }
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

fn start_core_background(runtime: Arc<Runtime>) {
    runtime.spawn(async {
        loop {
            elgato::initialise_devices().await;
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        }
    });

    runtime.spawn(async {
        plugins::initialise_plugins();
        application_watcher::init_application_watcher();
    });
}

struct RiverDeckApp {
    runtime: Arc<Runtime>,
    ui_events: Option<broadcast::Receiver<ui::UiEvent>>,

    // Keep the lock file alive for the lifetime of the app.
    #[allow(dead_code)]
    _lock_file: std::fs::File,

    start_hidden: bool,
    update_info: Arc<Mutex<Option<UpdateInfo>>>,

    #[cfg(feature = "tray")]
    tray: Option<TrayState>,
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
    action_search: String,
    texture_cache: HashMap<String, CachedTexture>,

    // Cached async data to avoid blocking the UI thread during paint.
    categories_cache: Option<Vec<(String, shared::Category)>>,
    categories_inflight: bool,
    categories_rx: Option<mpsc::Receiver<std::collections::HashMap<String, shared::Category>>>,

    plugins_cache: Option<Vec<riverdeck_core::api::plugins::PluginInfo>>,
    plugins_inflight: bool,
    plugins_rx: Option<mpsc::Receiver<Vec<riverdeck_core::api::plugins::PluginInfo>>>,

    pages_cache: HashMap<(String, String), Vec<String>>, // (device_id, profile_id) -> pages
    pages_inflight: Option<(String, String)>,
    pages_rx: Option<PagesRx>,

    action_controller_filter: String,
    drag_payload: Option<shared::Action>,
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

    // Action editor / Property Inspector (PI) window state.
    pi_child: Option<std::process::Child>,
    pi_for_context: Option<shared::ActionContext>,
    pi_last_error: Option<String>,

    // Marketplace window state (a simple webview window via `riverdeck-pi`).
    marketplace_child: Option<std::process::Child>,
    marketplace_last_error: Option<String>,

    // Profile management UI.
    show_profile_editor: bool,
    profile_name_input: String,
    profile_error: Option<String>,

    // Plugin management UI.
    show_manage_plugins: bool,
    plugin_manage_error: Option<String>,
    pending_plugin_install_pick: Option<mpsc::Receiver<Option<PathBuf>>>,
    pending_plugin_install_result: Option<mpsc::Receiver<Result<(), String>>>,

    // Non-blocking file picking (run dialogs off the UI thread).
    pending_icon_pick: Option<(mpsc::Receiver<Option<PathBuf>>, shared::ActionContext, u16)>,
    pending_screen_bg_pick: Option<(mpsc::Receiver<Option<PathBuf>>, String, String)>, // (rx, device_id, profile_id)
}

type PagesKey = (String, String);
type PagesRxPayload = (PagesKey, Vec<String>);
type PagesRx = mpsc::Receiver<PagesRxPayload>;

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectedSlot {
    controller: String,
    position: u8,
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
    fn new(
        runtime: Arc<Runtime>,
        lock_file: std::fs::File,
        start_hidden: bool,
        update_info: Arc<Mutex<Option<UpdateInfo>>>,
    ) -> Self {
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

        Self {
            runtime,
            ui_events: ui::subscribe(),
            _lock_file: lock_file,
            start_hidden,
            update_info,
            #[cfg(feature = "tray")]
            tray,
            #[cfg(feature = "tray")]
            hide_to_tray_requested: false,
            selected_device: None,
            selected_slot: None,
            button_label_context: None,
            button_label_input: String::new(),
            button_label_placement: shared::TextPlacement::Bottom,
            button_show_title: true,
            button_show_action_name: true,
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
            pi_child: None,
            pi_for_context: None,
            pi_last_error: None,
            marketplace_child: None,
            marketplace_last_error: None,
            show_profile_editor: false,
            profile_name_input: String::new(),
            profile_error: None,
            show_manage_plugins: false,
            plugin_manage_error: None,
            pending_plugin_install_pick: None,
            pending_plugin_install_result: None,
            pending_icon_pick: None,
            pending_screen_bg_pick: None,
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

                        ui.add_space(6.0);
                        ui.label("Button title (dynamic):");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.button_label_input)
                                .hint_text("Leave empty for clean"),
                        );

                        ui.horizontal(|ui| {
                            ui.checkbox(&mut self.button_show_action_name, "Show action name");
                            ui.checkbox(&mut self.button_show_title, "Show title");
                        });

                        ui.horizontal(|ui| {
                            ui.label("Placement:");
                            let placement_locked =
                                self.button_show_title && self.button_show_action_name;
                            ui.add_enabled_ui(!placement_locked, |ui| {
                                egui::ComboBox::from_id_salt("button_label_placement")
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
                            // Placement changes are staged; saved via the explicit Save button below.
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
                                    let _ = riverdeck_core::api::instances::set_button_label(
                                        ctx_to_set.clone(),
                                        text,
                                    )
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

                        let has_pi = !instance.action.property_inspector.trim().is_empty();
                        let open_for_this = self
                            .pi_for_context
                            .as_ref()
                            .is_some_and(|c| c == &instance.context)
                            && self.pi_child.is_some();

                        ui.horizontal(|ui| {
                            // Reuse existing PI controls from the original panel.
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
                                .add_enabled(has_pi && !open_for_this, egui::Button::new("Open PI"))
                                .clicked()
                            {
                                let dock = Self::compute_pi_dock_geometry(ctx, 420);
                                if let Err(err) =
                                    self.open_property_inspector_for_instance(device, instance, dock)
                                {
                                    self.pi_last_error = Some(err.to_string());
                                }
                            }
                            if ui
                                .add_enabled(open_for_this, egui::Button::new("Close PI"))
                                .clicked()
                            {
                                self.close_pi();
                            }

                            if !has_pi {
                                ui.label("This action has no Property Inspector.");
                            } else if open_for_this {
                                ui.label("PI open.");
                            }
                        });
                    }
                });
            });

        if let Some(err) = self.pi_last_error.as_ref() {
            ui.colored_label(ui.visuals().error_fg_color, err);
        }
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

    fn ellipsize_preview(s: &str, max_chars: usize) -> String {
        if max_chars == 0 {
            return String::new();
        }
        let chars: Vec<char> = s.chars().collect();
        if chars.len() <= max_chars {
            return s.to_owned();
        }
        if max_chars <= 3 {
            return "...".chars().take(max_chars).collect();
        }
        let keep = max_chars.saturating_sub(3);
        chars.into_iter().take(keep).collect::<String>() + "..."
    }

    fn wrap_words_preview(text: &str, max_cols: usize, max_lines: usize) -> Vec<String> {
        if max_cols == 0 || max_lines == 0 {
            return vec![];
        }
        let mut lines: Vec<String> = Vec::new();
        let mut current = String::new();

        let push_line = |line: String, lines: &mut Vec<String>| {
            if !line.trim().is_empty() {
                lines.push(line);
            }
        };

        for word in text.split_whitespace() {
            if lines.len() >= max_lines {
                break;
            }
            let sep = if current.is_empty() { "" } else { " " };
            let candidate = format!("{current}{sep}{word}");
            if candidate.chars().count() <= max_cols {
                current = candidate;
                continue;
            }

            if !current.is_empty() {
                push_line(std::mem::take(&mut current), &mut lines);
                if lines.len() >= max_lines {
                    break;
                }
            }

            // Hard-break long words.
            let mut remaining = word;
            while !remaining.is_empty() && lines.len() < max_lines {
                let chunk: String = remaining.chars().take(max_cols).collect();
                let taken = chunk.chars().count();
                push_line(chunk, &mut lines);
                remaining = &remaining[remaining
                    .char_indices()
                    .nth(taken)
                    .map(|(i, _)| i)
                    .unwrap_or(remaining.len())..];
            }
        }

        if lines.len() < max_lines && !current.is_empty() {
            push_line(current, &mut lines);
        }

        lines
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
            if !(cand.ends_with(".png") || cand.ends_with(".jpg") || cand.ends_with(".jpeg")) {
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

            let img = image::open(&resolved).ok()?.into_rgba8();
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
                                .add_filter("Image", &["png", "jpg", "jpeg"])
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

    fn close_pi(&mut self) {
        if let Some(mut child) = self.pi_child.take() {
            riverdeck_core::runtime_processes::unrecord_process(child.id());
            let _ = child.kill();
            let _ = child.wait();
        }
        self.pi_for_context = None;
    }

    fn close_marketplace(&mut self) {
        if let Some(mut child) = self.marketplace_child.take() {
            riverdeck_core::runtime_processes::unrecord_process(child.id());
            let _ = child.kill();
            let _ = child.wait();
        }
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

    fn open_marketplace(&mut self, ctx: &egui::Context) -> anyhow::Result<()> {
        // If already open, do nothing (best effort).
        if self.marketplace_child.is_some() {
            return Ok(());
        }

        // Open the marketplace in a dedicated webview window.
        // Note: the Elgato marketplace uses `streamdeck://...` deep links. We rely on the
        // webview's navigation handler (in `riverdeck-pi`) + RiverDeck's startup arg handler.
        let marketplace_url = "https://marketplace.elgato.com/stream-deck/plugins";
        let dock = Self::compute_marketplace_geometry(ctx);

        let child = match spawn_web_process("Elgato Marketplace", marketplace_url, dock) {
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

    fn compute_pi_dock_geometry(
        ctx: &egui::Context,
        desired_h: i32,
    ) -> Option<(i32, i32, i32, i32)> {
        let outer = ctx.input(|i| i.viewport().outer_rect)?;
        let w = outer.width().round().max(200.0) as i32;
        let x = outer.min.x.round() as i32;
        let y = outer.max.y.round() as i32;
        Some((x, y, w, desired_h.max(200)))
    }

    fn open_property_inspector_for_instance(
        &mut self,
        device: &shared::DeviceInfo,
        instance: &shared::ActionInstance,
        dock: Option<(i32, i32, i32, i32)>,
    ) -> anyhow::Result<()> {
        if instance.action.property_inspector.trim().is_empty() {
            return Err(anyhow::anyhow!("action has no property inspector"));
        }

        // If already open for this context, do nothing.
        if self
            .pi_for_context
            .as_ref()
            .is_some_and(|c| c == &instance.context)
            && self.pi_child.is_some()
        {
            return Ok(());
        }

        // Close any previous PI.
        self.close_pi();

        let port_base = *riverdeck_core::plugins::PORT_BASE;
        let manifest = riverdeck_core::plugins::manifest::read_manifest(
            &shared::config_dir()
                .join("plugins")
                .join(&instance.action.plugin),
        )?;
        let info = self.runtime.block_on(async {
            riverdeck_core::plugins::info_param::make_info(
                instance.action.plugin.clone(),
                manifest.version,
                false,
            )
            .await
        });

        let coordinates = if instance.context.controller == "Encoder" {
            serde_json::json!({ "row": 0, "column": instance.context.position })
        } else {
            serde_json::json!({
                "row": (instance.context.position / device.columns),
                "column": (instance.context.position % device.columns)
            })
        };

        let connect_payload = serde_json::json!({
            "action": instance.action.uuid,
            "context": instance.context.to_string(),
            "device": device.id,
            "payload": {
                "settings": instance.settings,
                "coordinates": coordinates,
                "controller": instance.context.controller,
                "state": instance.current_state,
                "isInMultiAction": instance.context.index != 0,
            }
        });

        let origin = format!("http://localhost:{}", port_base + 2);
        let pi_token = riverdeck_core::plugins::property_inspector_token();
        let pi_src = format!(
            "{origin}/{}|riverdeck_property_inspector?riverdeck_token={pi_token}",
            instance.action.property_inspector
        );
        let label = format!("pi_{}", instance.context.to_string().replace('.', "_"));

        let info_json = serde_json::to_string(&info)?;
        let connect_json = serde_json::to_string(&connect_payload)?;

        let child = spawn_pi_process(
            &label,
            &pi_src,
            &origin,
            port_base,
            &instance.context.to_string(),
            &info_json,
            &connect_json,
            dock,
        )?;

        self.pi_child = Some(child);
        self.pi_for_context = Some(instance.context.clone());
        self.pi_last_error = None;

        // Best-effort: notify plugin that PI appeared.
        self.runtime.block_on(async {
            let _ = riverdeck_core::events::outbound::property_inspector::property_inspector_did_appear(
                instance.context.clone(),
                "propertyInspectorDidAppear",
            )
            .await;
        });

        Ok(())
    }

    fn draw_action_row(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        action: &shared::Action,
        row_size: egui::Vec2,
        dragging: bool,
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
        painter.text(
            egui::pos2(text_rect.min.x, text_rect.center().y),
            egui::Align2::LEFT_CENTER,
            action.name.trim(),
            egui::FontId::proportional(13.0),
            visuals.text_color(),
        );

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
        slot: &SelectedSlot,
        instance: Option<&shared::ActionInstance>,
        selected: bool,
        drag_action: Option<&shared::Action>,
    ) -> egui::Response {
        let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
        let painter = ui.painter_at(rect);

        let pointer_pos = ctx.input(|i| i.pointer.latest_pos());
        let drop_hovered = pointer_pos.is_some_and(|p| rect.contains(p));
        let drop_valid =
            drag_action.is_some_and(|a| a.controllers.iter().any(|c| c == &slot.controller));
        if drag_action.is_some() && drop_hovered {
            self.drag_hover_slot = Some(slot.clone());
            self.drag_hover_valid = drop_valid;
        }

        let stroke = if selected || (drag_action.is_some() && drop_hovered && drop_valid) {
            egui::Stroke::new(2.0, ui.visuals().selection.stroke.color)
        } else if drag_action.is_some() && drop_hovered && !drop_valid {
            egui::Stroke::new(2.0, ui.visuals().error_fg_color)
        } else {
            ui.visuals().widgets.inactive.bg_stroke
        };
        let fill = if drag_action.is_some() && drop_hovered {
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
            let state_img = instance
                .states
                .get(instance.current_state as usize)
                .map(|s| s.image.as_str())
                .unwrap_or(instance.action.icon.as_str());

            if let Some(tex) = self.texture_for_path(ctx, state_img) {
                let inner = rect.shrink(6.0);
                painter.image(
                    tex.id(),
                    inner,
                    egui::Rect::from_min_max(egui::Pos2::new(0.0, 0.0), egui::Pos2::new(1.0, 1.0)),
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

            // Overlay label preview (matches device behavior).
            if let Some(st) = instance.states.get(instance.current_state as usize) {
                let title = st.text.trim();
                let action_name = instance.action.name.trim();
                let show_title = st.show && !title.is_empty();
                let show_action_name = st.show_action_name && !action_name.is_empty();

                let fg = egui::Color32::WHITE;

                // Helper macro to render text at a placement
                macro_rules! render_text_at_placement {
                    ($text:expr, $placement:expr) => {
                        match $placement {
                            shared::TextPlacement::Top => {
                                let r = egui::Rect::from_min_max(
                                    rect.min + egui::vec2(4.0, 4.0),
                                    egui::pos2(rect.max.x - 4.0, rect.min.y + 32.0),
                                );
                                let sizes = [11.0, 10.0, 9.0, 8.0, 7.0];
                                for sz in sizes {
                                    let font = egui::FontId::monospace(sz);
                                    let char_w = (sz * 0.60).max(1.0);
                                    let line_h = (sz + 2.0).max(1.0);
                                    let max_cols =
                                        ((r.width() - 6.0) / char_w).floor().max(1.0) as usize;
                                    let max_lines =
                                        ((r.height() - 6.0) / line_h).floor().max(1.0) as usize;
                                    let max_lines = max_lines.min(2);
                                    let mut lines = RiverDeckApp::wrap_words_preview(
                                        $text, max_cols, max_lines,
                                    );
                                    if lines.len() == max_lines && max_lines > 0 {
                                        if let Some(last) = lines.last_mut() {
                                            *last = RiverDeckApp::ellipsize_preview(last, max_cols);
                                        }
                                    }
                                    let t = lines.join("\n");
                                    let center = r.center();
                                    painter.text(
                                        center + egui::vec2(1.0, 1.0),
                                        egui::Align2::CENTER_CENTER,
                                        t.clone(),
                                        font.clone(),
                                        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 200),
                                    );
                                    painter.text(center, egui::Align2::CENTER_CENTER, t, font, fg);
                                    break;
                                }
                            }
                            shared::TextPlacement::Bottom => {
                                let r = egui::Rect::from_min_max(
                                    egui::pos2(rect.min.x + 4.0, rect.max.y - 32.0),
                                    rect.max - egui::vec2(4.0, 4.0),
                                );
                                let sizes = [11.0, 10.0, 9.0, 8.0, 7.0];
                                for sz in sizes {
                                    let font = egui::FontId::monospace(sz);
                                    let char_w = (sz * 0.60).max(1.0);
                                    let line_h = (sz + 2.0).max(1.0);
                                    let max_cols =
                                        ((r.width() - 6.0) / char_w).floor().max(1.0) as usize;
                                    let max_lines =
                                        ((r.height() - 6.0) / line_h).floor().max(1.0) as usize;
                                    let max_lines = max_lines.min(2);
                                    let mut lines = RiverDeckApp::wrap_words_preview(
                                        $text, max_cols, max_lines,
                                    );
                                    if lines.len() == max_lines && max_lines > 0 {
                                        if let Some(last) = lines.last_mut() {
                                            *last = RiverDeckApp::ellipsize_preview(last, max_cols);
                                        }
                                    }
                                    let t = lines.join("\n");
                                    let center = r.center();
                                    painter.text(
                                        center + egui::vec2(1.0, 1.0),
                                        egui::Align2::CENTER_CENTER,
                                        t.clone(),
                                        font.clone(),
                                        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 200),
                                    );
                                    painter.text(center, egui::Align2::CENTER_CENTER, t, font, fg);
                                    break;
                                }
                            }
                            shared::TextPlacement::Left => {
                                let r = egui::Rect::from_min_max(
                                    rect.min + egui::vec2(4.0, 4.0),
                                    egui::pos2(rect.min.x + 18.0, rect.max.y - 4.0),
                                );
                                let sizes = [10.0, 9.0, 8.0, 7.0];
                                for sz in sizes {
                                    let font = egui::FontId::monospace(sz);
                                    let line_h = (sz + 1.0).max(1.0);
                                    let max_chars =
                                        ((r.height() - 6.0) / line_h).floor().max(1.0) as usize;
                                    let vertical = $text
                                        .chars()
                                        .take(max_chars)
                                        .map(|c| c.to_string())
                                        .collect::<Vec<_>>()
                                        .join("\n");
                                    let center = r.center();
                                    painter.text(
                                        center + egui::vec2(1.0, 1.0),
                                        egui::Align2::CENTER_CENTER,
                                        vertical.clone(),
                                        font.clone(),
                                        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 200),
                                    );
                                    painter.text(
                                        center,
                                        egui::Align2::CENTER_CENTER,
                                        vertical,
                                        font,
                                        fg,
                                    );
                                    break;
                                }
                            }
                            shared::TextPlacement::Right => {
                                let r = egui::Rect::from_min_max(
                                    egui::pos2(rect.max.x - 18.0, rect.min.y + 4.0),
                                    rect.max - egui::vec2(4.0, 4.0),
                                );
                                let sizes = [10.0, 9.0, 8.0, 7.0];
                                for sz in sizes {
                                    let font = egui::FontId::monospace(sz);
                                    let line_h = (sz + 1.0).max(1.0);
                                    let max_chars =
                                        ((r.height() - 6.0) / line_h).floor().max(1.0) as usize;
                                    let vertical = $text
                                        .chars()
                                        .take(max_chars)
                                        .map(|c| c.to_string())
                                        .collect::<Vec<_>>()
                                        .join("\n");
                                    let center = r.center();
                                    painter.text(
                                        center + egui::vec2(1.0, 1.0),
                                        egui::Align2::CENTER_CENTER,
                                        vertical.clone(),
                                        font.clone(),
                                        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 200),
                                    );
                                    painter.text(
                                        center,
                                        egui::Align2::CENTER_CENTER,
                                        vertical,
                                        font,
                                        fg,
                                    );
                                    break;
                                }
                            }
                        }
                    };
                }

                // If both are enabled (and different), force Top/Bottom.
                if show_title && show_action_name && title != action_name {
                    render_text_at_placement!(action_name, shared::TextPlacement::Top);
                    render_text_at_placement!(title, shared::TextPlacement::Bottom);
                } else {
                    // Keep legacy behavior: the Stream Deck "Title" uses `text_placement`.
                    if show_title {
                        render_text_at_placement!(title, st.text_placement);
                    }

                    // If the title already equals the action name (common default), don't render both.
                    if show_action_name && (!show_title || title != action_name) {
                        let opposite = |p: shared::TextPlacement| match p {
                            shared::TextPlacement::Top => shared::TextPlacement::Bottom,
                            shared::TextPlacement::Bottom => shared::TextPlacement::Top,
                            shared::TextPlacement::Left => shared::TextPlacement::Right,
                            shared::TextPlacement::Right => shared::TextPlacement::Left,
                        };
                        let placement = if show_title {
                            opposite(st.text_placement)
                        } else {
                            // If there's no title, keep the action name in the familiar place.
                            shared::TextPlacement::Bottom
                        };
                        render_text_at_placement!(action_name, placement);
                    }
                }
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
        slot: &SelectedSlot,
        instance: Option<&shared::ActionInstance>,
        selected: bool,
        drag_action: Option<&shared::Action>,
    ) -> egui::Response {
        let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
        let painter = ui.painter_at(rect);

        let pointer_pos = ctx.input(|i| i.pointer.latest_pos());
        let drop_hovered = pointer_pos.is_some_and(|p| rect.contains(p));
        let drop_valid =
            drag_action.is_some_and(|a| a.controllers.iter().any(|c| c == &slot.controller));
        if drag_action.is_some() && drop_hovered {
            self.drag_hover_slot = Some(slot.clone());
            self.drag_hover_valid = drop_valid;
        }

        let visuals = ui.visuals();
        let center = rect.center();
        let radius = (rect.width().min(rect.height()) * 0.5) - 2.0;

        let stroke = if selected || (drag_action.is_some() && drop_hovered && drop_valid) {
            egui::Stroke::new(2.0, visuals.selection.stroke.color)
        } else if drag_action.is_some() && drop_hovered && !drop_valid {
            egui::Stroke::new(2.0, visuals.error_fg_color)
        } else {
            visuals.widgets.inactive.bg_stroke
        };
        let fill = if drag_action.is_some() && drop_hovered {
            visuals.widgets.hovered.bg_fill
        } else {
            visuals.extreme_bg_color
        };

        // Dial body
        painter.circle_filled(center, radius, fill);
        painter.circle_stroke(center, radius, stroke);

        // Optional: render action image/icon in an inscribed square.
        if let Some(instance) = instance {
            let state_img = instance
                .states
                .get(instance.current_state as usize)
                .map(|s| s.image.as_str())
                .unwrap_or(instance.action.icon.as_str());

            if let Some(tex) = self.texture_for_path(ctx, state_img) {
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

            // Overlay label preview (matches device behavior).
            if let Some(st) = instance.states.get(instance.current_state as usize) {
                let title = st.text.trim();
                let action_name = instance.action.name.trim();
                let show_title = st.show && !title.is_empty();
                let show_action_name = st.show_action_name && !action_name.is_empty();

                let fg = egui::Color32::WHITE;

                // Helper macro to render text at a placement
                macro_rules! render_text_at_placement {
                    ($text:expr, $placement:expr) => {
                        match $placement {
                            shared::TextPlacement::Top => {
                                let r = egui::Rect::from_center_size(
                                    egui::pos2(rect.center().x, rect.min.y + 10.0),
                                    egui::vec2(rect.width() - 8.0, 26.0),
                                );
                                let sizes = [10.0, 9.0, 8.0, 7.0];
                                for sz in sizes {
                                    let font = egui::FontId::monospace(sz);
                                    let char_w = (sz * 0.60).max(1.0);
                                    let line_h = (sz + 2.0).max(1.0);
                                    let max_cols =
                                        ((r.width() - 6.0) / char_w).floor().max(1.0) as usize;
                                    let max_lines =
                                        ((r.height() - 6.0) / line_h).floor().max(1.0) as usize;
                                    let max_lines = max_lines.min(2);
                                    let mut lines = RiverDeckApp::wrap_words_preview(
                                        $text, max_cols, max_lines,
                                    );
                                    if lines.len() == max_lines && max_lines > 0 {
                                        if let Some(last) = lines.last_mut() {
                                            *last = RiverDeckApp::ellipsize_preview(last, max_cols);
                                        }
                                    }
                                    let t = lines.join("\n");
                                    let center = r.center();
                                    painter.text(
                                        center + egui::vec2(1.0, 1.0),
                                        egui::Align2::CENTER_CENTER,
                                        t.clone(),
                                        font.clone(),
                                        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 200),
                                    );
                                    painter.text(center, egui::Align2::CENTER_CENTER, t, font, fg);
                                    break;
                                }
                            }
                            shared::TextPlacement::Bottom => {
                                let r = egui::Rect::from_center_size(
                                    egui::pos2(rect.center().x, rect.max.y - 10.0),
                                    egui::vec2(rect.width() - 8.0, 26.0),
                                );
                                let sizes = [10.0, 9.0, 8.0, 7.0];
                                for sz in sizes {
                                    let font = egui::FontId::monospace(sz);
                                    let char_w = (sz * 0.60).max(1.0);
                                    let line_h = (sz + 2.0).max(1.0);
                                    let max_cols =
                                        ((r.width() - 6.0) / char_w).floor().max(1.0) as usize;
                                    let max_lines =
                                        ((r.height() - 6.0) / line_h).floor().max(1.0) as usize;
                                    let max_lines = max_lines.min(2);
                                    let mut lines = RiverDeckApp::wrap_words_preview(
                                        $text, max_cols, max_lines,
                                    );
                                    if lines.len() == max_lines && max_lines > 0 {
                                        if let Some(last) = lines.last_mut() {
                                            *last = RiverDeckApp::ellipsize_preview(last, max_cols);
                                        }
                                    }
                                    let t = lines.join("\n");
                                    let center = r.center();
                                    painter.text(
                                        center + egui::vec2(1.0, 1.0),
                                        egui::Align2::CENTER_CENTER,
                                        t.clone(),
                                        font.clone(),
                                        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 200),
                                    );
                                    painter.text(center, egui::Align2::CENTER_CENTER, t, font, fg);
                                    break;
                                }
                            }
                            shared::TextPlacement::Left => {
                                let r = egui::Rect::from_center_size(
                                    egui::pos2(rect.min.x + 10.0, rect.center().y),
                                    egui::vec2(16.0, rect.height() - 8.0),
                                );
                                let sizes = [9.0, 8.0, 7.0];
                                for sz in sizes {
                                    let font = egui::FontId::monospace(sz);
                                    let line_h = (sz + 1.0).max(1.0);
                                    let max_chars =
                                        ((r.height() - 6.0) / line_h).floor().max(1.0) as usize;
                                    let vertical = $text
                                        .chars()
                                        .take(max_chars)
                                        .map(|c| c.to_string())
                                        .collect::<Vec<_>>()
                                        .join("\n");
                                    let center = r.center();
                                    painter.text(
                                        center + egui::vec2(1.0, 1.0),
                                        egui::Align2::CENTER_CENTER,
                                        vertical.clone(),
                                        font.clone(),
                                        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 200),
                                    );
                                    painter.text(
                                        center,
                                        egui::Align2::CENTER_CENTER,
                                        vertical,
                                        font,
                                        fg,
                                    );
                                    break;
                                }
                            }
                            shared::TextPlacement::Right => {
                                let r = egui::Rect::from_center_size(
                                    egui::pos2(rect.max.x - 10.0, rect.center().y),
                                    egui::vec2(16.0, rect.height() - 8.0),
                                );
                                let sizes = [9.0, 8.0, 7.0];
                                for sz in sizes {
                                    let font = egui::FontId::monospace(sz);
                                    let line_h = (sz + 1.0).max(1.0);
                                    let max_chars =
                                        ((r.height() - 6.0) / line_h).floor().max(1.0) as usize;
                                    let vertical = $text
                                        .chars()
                                        .take(max_chars)
                                        .map(|c| c.to_string())
                                        .collect::<Vec<_>>()
                                        .join("\n");
                                    let center = r.center();
                                    painter.text(
                                        center + egui::vec2(1.0, 1.0),
                                        egui::Align2::CENTER_CENTER,
                                        vertical.clone(),
                                        font.clone(),
                                        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 200),
                                    );
                                    painter.text(
                                        center,
                                        egui::Align2::CENTER_CENTER,
                                        vertical,
                                        font,
                                        fg,
                                    );
                                    break;
                                }
                            }
                        }
                    };
                }

                // If both are enabled (and different), force Top/Bottom.
                if show_title && show_action_name && title != action_name {
                    render_text_at_placement!(action_name, shared::TextPlacement::Top);
                    render_text_at_placement!(title, shared::TextPlacement::Bottom);
                } else {
                    // Keep legacy behavior: the Stream Deck "Title" uses `text_placement`.
                    if show_title {
                        render_text_at_placement!(title, st.text_placement);
                    }

                    // If the title already equals the action name (common default), don't render both.
                    if show_action_name && (!show_title || title != action_name) {
                        let opposite = |p: shared::TextPlacement| match p {
                            shared::TextPlacement::Top => shared::TextPlacement::Bottom,
                            shared::TextPlacement::Bottom => shared::TextPlacement::Top,
                            shared::TextPlacement::Left => shared::TextPlacement::Right,
                            shared::TextPlacement::Right => shared::TextPlacement::Left,
                        };
                        let placement = if show_title {
                            opposite(st.text_placement)
                        } else {
                            // If there's no title, keep the action name in the familiar place.
                            shared::TextPlacement::Bottom
                        };
                        render_text_at_placement!(action_name, placement);
                    }
                }
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
                Ok(_event) => {
                    // For the initial shell we simply wake UI; state is pulled from core singletons.
                }
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(broadcast::error::TryRecvError::Closed) => {
                    self.ui_events = None;
                    break;
                }
            }
        }
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
            while gtk::glib::MainContext::default().iteration(false) {}
        }

        // If the user tries to close the window, prefer "hide to tray" (when available).
        // A real exit is done via the tray menu (Quit).
        #[cfg(feature = "tray")]
        if ctx.input(|i| i.viewport().close_requested()) {
            if QUIT_REQUESTED.load(Ordering::SeqCst) {
                // Allow the close to proceed.
            } else if self.tray.is_some() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.hide_to_tray_requested = true;
            }
        }

        if self.start_hidden {
            self.start_hidden = false;
            // If tray is available, start hidden-to-tray (no taskbar entry).
            // Otherwise, fall back to minimizing so the user can still find the app.
            #[cfg(feature = "tray")]
            if self.tray.is_some() {
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

        // Apply hide-to-tray requests (we do this after polling the tray menu so that a
        // "Show" click can't be immediately overridden by a stale request).
        #[cfg(feature = "tray")]
        if self.hide_to_tray_requested {
            self.hide_to_tray_requested = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
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
            || self.pending_plugin_install_pick.is_some()
            || self.pending_plugin_install_result.is_some()
            || self.pending_icon_pick.is_some()
            || self.pending_screen_bg_pick.is_some()
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        // Plugin install flow (non-blocking).
        if let Some(rx) = self.pending_plugin_install_pick.as_ref()
            && let Ok(picked) = rx.try_recv()
        {
            self.pending_plugin_install_pick = None;
            if let Some(path) = picked {
                let path = path.to_string_lossy().into_owned();
                let (tx, rx_done) = mpsc::channel();
                self.pending_plugin_install_result = Some(rx_done);
                self.runtime.spawn(async move {
                    let res = riverdeck_core::api::plugins::install_plugin(None, Some(path), None)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx.send(res);
                });
            }
        }
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

        // Reset per-frame drag hover state.
        self.drag_hover_slot = None;
        self.drag_hover_valid = false;

        // Avoid borrow conflicts: take a local snapshot of the drag payload for this frame.
        let drag_action = self.drag_payload.clone();

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
                egui::Window::new(format!("Update available: {}", info.tag))
                    .open(&mut self.show_update_details)
                    .show(ctx, |ui| {
                        egui::ScrollArea::vertical()
                            .max_height(300.0)
                            .show(ui, |ui| {
                                ui.label(&info.body);
                            });
                        ui.label("Update via your package manager / release download.");
                    });
            } else {
                self.show_update_details = false;
            }
        }

        egui::SidePanel::left("devices").show(ctx, |ui| {
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
                if ui.button("Elgato Marketplace").clicked()
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

        // If the current selection no longer matches the open PI (device/profile switched, etc.),
        // close it to avoid drifting state.
        if let Some(open_ctx) = self.pi_for_context.clone() {
            let still_selected = self.selected_slot.as_ref().is_some_and(|slot| {
                selected_device
                    .as_ref()
                    .is_some_and(|dev| dev.id == open_ctx.device)
                    && selected_profile == open_ctx.profile
                    && slot.controller == open_ctx.controller
                    && slot.position == open_ctx.position
            });
            if !still_selected {
                self.close_pi();
            }
        }

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
            .default_width(560.0)
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
                // Clone for this frame to avoid borrowing `self` across the ScrollArea closure
                // (we need `&mut self` inside for `draw_action_row`).
                let cats = self.categories_cache.clone().unwrap_or_default();

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
                                        self.drag_payload.as_ref().is_some_and(|a| {
                                            a.uuid == action.uuid && a.plugin == action.plugin
                                        });
                                    let row_size = egui::vec2(ui.available_width(), row_height);
                                    let resp = self.draw_action_row(
                                        ui,
                                        ctx,
                                        action,
                                        row_size,
                                        dragging_this,
                                    );
                                    if resp.drag_started() {
                                        self.drag_payload = Some(action.clone());
                                    }
                                }
                            });
                    }
                });
            });

        // Bottom action editor: hidden when no slot is selected, otherwise a centered tab that
        // expands into a full-width bottom sheet (collapsed by default).
        let editor_visible =
            selected_device.is_some() && snapshot.is_some() && self.selected_slot.is_some();
        if !editor_visible {
            self.action_editor_open = false;
        } else if let (Some(device), Some(snapshot), Some(slot)) = (
            &selected_device,
            snapshot.as_ref(),
            self.selected_slot.clone(),
        ) {
            if !self.action_editor_open {
                egui::TopBottomPanel::bottom("action_editor_tab")
                    .exact_height(44.0)
                    .frame(egui::Frame::NONE.inner_margin(egui::Margin::same(6)))
                    .show(ctx, |ui| {
                        ui.vertical_centered(|ui| {
                            let tab_w = ui.available_width().clamp(240.0, 520.0);
                            let tab_h = 34.0;
                            let (rect, resp) = ui.allocate_exact_size(
                                egui::vec2(tab_w.min(ui.available_width()), tab_h),
                                egui::Sense::click(),
                            );

                            let rounding = egui::CornerRadius::same(12);
                            let fill = if resp.hovered() {
                                ui.visuals().widgets.hovered.bg_fill
                            } else {
                                ui.visuals().widgets.inactive.bg_fill
                            };
                            let stroke = if resp.hovered() {
                                ui.visuals().widgets.hovered.bg_stroke
                            } else {
                                ui.visuals().widgets.inactive.bg_stroke
                            };

                            ui.painter().rect(
                                rect,
                                rounding,
                                fill,
                                stroke,
                                egui::StrokeKind::Inside,
                            );

                            let label =
                                format!("Action Editor — {} {}", slot.controller, slot.position);
                            ui.painter().text(
                                rect.center(),
                                egui::Align2::CENTER_CENTER,
                                label,
                                egui::FontId::proportional(13.0),
                                ui.visuals().text_color(),
                            );

                            if resp.clicked() {
                                self.action_editor_open = true;
                            }
                        });
                    });
            } else {
                egui::TopBottomPanel::bottom("action_editor_sheet")
                    .default_height(260.0)
                    .resizable(true)
                    .show(ctx, |ui| {
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.heading("Action Editor");
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui.button("▾").on_hover_text("Collapse").clicked() {
                                            self.action_editor_open = false;
                                        }
                                    },
                                );
                            });
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
                egui::Window::new("Profile")
                    .open(&mut open)
                    .collapsible(false)
                    .resizable(false)
                    .show(ctx, |ui| {
                        ui.label("Name:");
                        ui.add(egui::TextEdit::singleline(&mut self.profile_name_input));
                        if let Some(err) = self.profile_error.as_ref() {
                            ui.colored_label(ui.visuals().error_fg_color, err);
                        }
                        ui.add_space(8.0);

                        let trimmed = self.profile_name_input.trim().to_owned();
                        let valid = !trimmed.is_empty();

                        ui.horizontal(|ui| {
                            if ui.add_enabled(valid, egui::Button::new("Create (empty)")).clicked()
                            {
                                let device_id = device.id.clone();
                                let new_id = trimmed.clone();
                                self.runtime.spawn(async move {
                                    let _ =
                                        riverdeck_core::api::profiles::create_profile(device_id.clone(), new_id.clone(), None)
                                            .await;
                                    let _ =
                                        riverdeck_core::api::profiles::set_selected_profile(device_id, new_id).await;
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
                                    let _ =
                                        riverdeck_core::api::profiles::set_selected_profile(device_id, new_id).await;
                                });
                                self.show_profile_editor = false;
                            }
                            if ui
                                .add_enabled(valid && selected_profile != "Default", egui::Button::new("Rename"))
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
                                    let _ =
                                        riverdeck_core::api::profiles::set_selected_profile(device_id, new_id).await;
                                });
                                self.show_profile_editor = false;
                            }
                            if ui.button("Cancel").clicked() {
                                self.show_profile_editor = false;
                            }
                        });
                    });
                if !open {
                    self.show_profile_editor = false;
                }
            }

            if self.show_settings {
                let mut open = true;
                egui::Window::new("Settings")
                    .open(&mut open)
                    .collapsible(false)
                    .resizable(false)
                    .show(ctx, |ui| {
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
                if !open {
                    self.show_settings = false;
                }
            }

            if self.show_manage_plugins {
                let mut open = true;
                egui::Window::new("Manage Plugins")
                    .open(&mut open)
                    .collapsible(false)
                    .resizable(true)
                    .default_width(520.0)
                    .show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            if ui.button("Install…").clicked()
                                && self.pending_plugin_install_pick.is_none()
                            {
                                let (tx, rx) = mpsc::channel();
                                std::thread::spawn(move || {
                                    let picked = rfd::FileDialog::new()
                                        .add_filter("Stream Deck plugins", &["streamDeckPlugin", "zip"])
                                        .pick_file();
                                    let _ = tx.send(picked);
                                });
                                self.pending_plugin_install_pick = Some(rx);
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
                        if self.pending_plugin_install_result.is_some() {
                            ui.label(
                                egui::RichText::new("Installing…")
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
                            self.runtime.spawn(async move {
                                let list =
                                    riverdeck_core::api::plugins::list_plugins().await.unwrap_or_default();
                                let _ = tx.send(list);
                            });
                        }
                        // Clone the display fields we need so we can call `texture_for_path` (needs `&mut self`)
                        // inside the ScrollArea closure without borrowing `self` immutably.
                        let plugins: Vec<(String, String, String, String, bool, bool)> = self
                            .plugins_cache
                            .as_ref()
                            .map(|v| {
                                v.iter()
                                    .map(|p| {
                                        (
                                            p.id.clone(),
                                            p.name.clone(),
                                            p.icon.clone(),
                                            p.version.clone(),
                                            p.builtin,
                                            p.registered,
                                        )
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
                            for (id, name, icon, version, builtin, registered) in plugins {
                                egui::Frame::group(ui.style())
                                    .corner_radius(egui::CornerRadius::same(10))
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            let icon_size = egui::vec2(28.0, 28.0);
                                            if let Some(tex) = self.texture_for_path(ctx, &icon) {
                                                ui.image((tex.id(), icon_size));
                                            } else {
                                                ui.allocate_exact_size(icon_size, egui::Sense::hover());
                                            }

                                            ui.vertical(|ui| {
                                                ui.label(egui::RichText::new(&name).strong());
                                                ui.label(
                                                    egui::RichText::new(format!("{id} • v{version}"))
                                                        .small()
                                                        .color(ui.visuals().weak_text_color()),
                                                );
                                                if !registered {
                                                    ui.label(
                                                        egui::RichText::new("Not running / not registered")
                                                            .small()
                                                            .color(ui.visuals().weak_text_color()),
                                                    );
                                                }
                                            });

                                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                let remove = ui.add_enabled(!builtin, egui::Button::new("Remove"));
                                                if remove.clicked() {
                                                    // Invalidate caches that depend on plugin discovery.
                                                    self.plugins_cache = None;
                                                    self.plugins_inflight = false;
                                                    self.plugins_rx = None;
                                                    self.categories_cache = None;
                                                    self.categories_inflight = false;
                                                    self.categories_rx = None;
                                                    let id = id.clone();
                                                    self.runtime.spawn(async move {
                                                        let _ = riverdeck_core::api::plugins::remove_plugin(id).await;
                                                        plugins::initialise_plugins();
                                                    });
                                                }
                                                if builtin {
                                                    ui.label(
                                                        egui::RichText::new("built-in")
                                                            .small()
                                                            .color(ui.visuals().weak_text_color()),
                                                    );
                                                }
                                            });
                                        });
                                    });
                                ui.add_space(6.0);
                            }
                        });
                    });
                if !open {
                    self.show_manage_plugins = false;
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
                                        &slot,
                                        instance,
                                        selected,
                                        drag_action.as_ref(),
                                    );
                                    if resp.clicked() {
                                        self.selected_slot = Some(slot);
                                        // Collapsed by default on selection.
                                        self.action_editor_open = false;
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
                                    &slot,
                                    instance,
                                    selected,
                                    drag_action.as_ref(),
                                );
                                if resp.clicked() {
                                    self.selected_slot = Some(slot);
                                    // Collapsed by default on selection.
                                    self.action_editor_open = false;
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
        if let Some(action) = self.drag_payload.clone() {
            let action_preview = action.clone();
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
                                    if let Some(path) = Self::action_icon_path(&action_preview)
                                        && let Some(tex) = self.texture_for_path(ctx, path)
                                    {
                                        ui.image((tex.id(), icon_size));
                                    }
                                    ui.label(&action_preview.name);
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
                    let ctx_to_set = shared::Context {
                        device: device.id.clone(),
                        profile: selected_profile.clone(),
                        page: page_id,
                        controller: slot.controller.clone(),
                        position: slot.position,
                    };
                    let _ = self.runtime.block_on(async {
                        riverdeck_core::api::instances::create_instance(action.clone(), ctx_to_set)
                            .await
                    });
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
    }
}

impl Drop for RiverDeckApp {
    fn drop(&mut self) {
        self.close_pi();
        self.close_marketplace();
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
            let buttons_w = 96.0;
            let gap = 6.0;

            let right_x0 = rect.right() - buttons_w;
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
            let right_rect =
                egui::Rect::from_min_max(egui::pos2(right_x0, rect.top()), rect.right_bottom());

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
                        ui.spacing_mut().item_spacing.x = 6.0;
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
                        // Hide to tray when available; otherwise close.
                        #[cfg(feature = "tray")]
                        if self.tray.is_some() {
                            self.hide_to_tray_requested = true;
                        } else {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        #[cfg(not(feature = "tray"))]
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

#[allow(clippy::too_many_arguments)]
fn spawn_pi_process(
    label: &str,
    pi_src: &str,
    origin: &str,
    port: u16,
    context: &str,
    info_json: &str,
    connect_json: &str,
    dock: Option<(i32, i32, i32, i32)>,
) -> anyhow::Result<std::process::Child> {
    let mut cmd = std::process::Command::new(resolve_pi_exe()?);
    cmd.arg("--label")
        .arg(label)
        .arg("--pi-src")
        .arg(pi_src)
        .arg("--origin")
        .arg(origin)
        .arg("--ws-port")
        .arg(port.to_string())
        .arg("--context")
        .arg(context)
        .arg("--info-json")
        .arg(info_json)
        .arg("--connect-json")
        .arg(connect_json);

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

    let child = cmd.spawn()?;
    riverdeck_core::runtime_processes::record_process(
        child.id(),
        "riverdeck_pi",
        vec![
            "--label".to_owned(),
            label.to_owned(),
            "--ws-port".to_owned(),
            port.to_string(),
        ],
    );
    Ok(child)
}

fn spawn_web_process(
    label: &str,
    url: &str,
    dock: Option<(i32, i32, i32, i32)>,
) -> anyhow::Result<std::process::Child> {
    let mut cmd = std::process::Command::new(resolve_pi_exe()?);
    cmd.arg("--label").arg(label).arg("--url").arg(url);

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

    let child = cmd.spawn()?;
    riverdeck_core::runtime_processes::record_process(
        child.id(),
        "riverdeck_pi_web",
        vec!["--label".to_owned(), label.to_owned(), "--url".to_owned()],
    );
    Ok(child)
}

fn pi_exe_basename() -> &'static str {
    if cfg!(windows) {
        "riverdeck-pi.exe"
    } else {
        "riverdeck-pi"
    }
}

fn resolve_pi_exe() -> anyhow::Result<std::ffi::OsString> {
    // 1) Prefer a sibling binary next to the current executable (packaged installs).
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join(pi_exe_basename());
        if candidate.exists() {
            return Ok(candidate.into_os_string());
        }
    }

    // 2) Fall back to PATH. This helps in dev when `riverdeck-pi` is built but not
    // located next to the `riverdeck-egui` binary.
    Ok(std::ffi::OsString::from(pi_exe_basename()))
}

fn is_process_not_found(err: &anyhow::Error) -> bool {
    err.downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}
