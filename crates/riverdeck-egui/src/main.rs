use riverdeck_core::{application_watcher, elgato, plugins, shared, ui};

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;
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
        let should_replace = replace_instance || cfg!(debug_assertions)
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
                log::warn!("RiverDeck already running; could not acquire lock after replacement attempt");
                return Ok(());
            }
        } else {
            log::warn!("RiverDeck already running (lockfile held). Pass `--replace` to take over.");
            return Ok(());
        }
    }

    // Record our PID into the lock file (best-effort).
    let _ = write_lock_pid(&mut lock_file);

    let (ui_tx, _ui_rx) = broadcast::channel(256);
    ui::init(ui_tx);

    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("riverdeck-core")
            .build()?,
    );
    start_core_background(runtime.clone());

    let update_info: Arc<Mutex<Option<UpdateInfo>>> = Arc::new(Mutex::new(None));
    start_update_check(runtime.clone(), update_info.clone());
    handle_startup_args(runtime.clone(), args.clone());

    let mut native_options = eframe::NativeOptions::default();
    // Exit the process when the main window is closed.
    // This prevents "background instances" that keep running after the window is gone.
    native_options.run_and_return = false;
    // On Linux, window button placement (left/right) is usually controlled by the window manager.
    // If we want "always top-right" regardless of WM settings, we need a custom title bar.
    //
    // NOTE: This intentionally only applies to Linux to avoid fighting platform conventions
    // (macOS uses top-left window controls).
    #[cfg(target_os = "linux")]
    {
        native_options.viewport = native_options.viewport.with_decorations(false);
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
    action_search: String,
    texture_cache: HashMap<String, CachedTexture>,

    show_update_details: bool,
}

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
    keys: Vec<Option<shared::ActionInstance>>,
    sliders: Vec<Option<shared::ActionInstance>>,
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
            action_search: String::new(),
            texture_cache: HashMap::new(),
            show_update_details: false,
        }
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

        if !(path.ends_with(".png") || path.ends_with(".jpg") || path.ends_with(".jpeg")) {
            return None;
        }

        let resolved = self.resolve_icon_path(path)?;
        let cache_key = resolved.to_string_lossy().into_owned();
        if !resolved.is_file() {
            self.texture_cache.remove(&cache_key);
            return None;
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
        Some(texture)
    }

    fn draw_slot_preview(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        size: egui::Vec2,
        slot: &SelectedSlot,
        instance: Option<&shared::ActionInstance>,
        selected: bool,
    ) -> egui::Response {
        let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
        let painter = ui.painter_at(rect);

        let stroke = if selected {
            egui::Stroke::new(2.0, ui.visuals().selection.stroke.color)
        } else {
            ui.visuals().widgets.inactive.bg_stroke
        };
        painter.rect(
            rect,
            egui::CornerRadius::same(6),
            ui.visuals().extreme_bg_color,
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

    fn load_profile_snapshot(
        &self,
        device: &shared::DeviceInfo,
        profile_id: &str,
    ) -> anyhow::Result<ProfileSnapshot> {
        self.runtime.block_on(async {
            let locks = riverdeck_core::store::profiles::acquire_locks().await;
            let store = locks.profile_stores.get_profile_store(device, profile_id)?;
            Ok(ProfileSnapshot {
                keys: store.value.keys.clone(),
                sliders: store.value.sliders.clone(),
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

impl eframe::App for RiverDeckApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            #[cfg(target_os = "linux")]
            {
                self.draw_custom_titlebar(ui, ctx);
            }

            #[cfg(not(target_os = "linux"))]
            {
                ui.horizontal(|ui| {
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
            ui.heading("Devices");

            for entry in shared::DEVICES.iter() {
                let id = entry.key().clone();
                let selected = self.selected_device.as_deref() == Some(&id);
                if ui
                    .selectable_label(selected, format!("{} ({})", entry.value().name, id))
                    .clicked()
                {
                    self.selected_device = Some(id);
                    self.selected_slot = None;
                }
            }
        });

        let selected_device = self
            .selected_device
            .as_ref()
            .and_then(|id| shared::DEVICES.get(id).map(|d| d.value().clone()));
        if self.selected_device.is_some() && selected_device.is_none() {
            self.selected_device = None;
            self.selected_slot = None;
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
                }
            } else if slot.position as usize >= key_count {
                self.selected_slot = None;
            }
        }

        egui::SidePanel::right("actions").show(ctx, |ui| {
            ui.heading("Actions");

            let Some(device) = &selected_device else {
                ui.label("Select a device.");
                return;
            };
            let Some(slot) = self.selected_slot.clone() else {
                ui.label("Select a key/dial in the preview to assign an action.");
                return;
            };

            let instance = snapshot.as_ref().and_then(|s| match &slot.controller[..] {
                "Encoder" => s
                    .sliders
                    .get(slot.position as usize)
                    .and_then(|v| v.as_ref()),
                _ => s.keys.get(slot.position as usize).and_then(|v| v.as_ref()),
            });

            ui.horizontal(|ui| {
                ui.monospace(format!("slot: {} {}", slot.controller, slot.position));
                if instance.is_some() && ui.button("Clear").clicked() {
                    let ctx_to_clear = shared::Context {
                        device: device.id.clone(),
                        profile: selected_profile.clone(),
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

            ui.add(
                egui::TextEdit::singleline(&mut self.action_search).hint_text("Search actions…"),
            );
            ui.separator();

            let categories = self
                .runtime
                .block_on(async { riverdeck_core::api::get_categories().await });
            let mut cats: Vec<_> = categories.into_iter().collect();
            cats.sort_by(|(a, _), (b, _)| a.cmp(b));

            let search = self.action_search.to_lowercase();
            egui::ScrollArea::vertical().show(ui, |ui| {
                for (cat_name, cat) in cats {
                    egui::CollapsingHeader::new(cat_name)
                        .default_open(false)
                        .show(ui, |ui| {
                            for action in cat.actions {
                                if !action.visible_in_action_list {
                                    continue;
                                }
                                if !action.controllers.iter().any(|c| c == &slot.controller) {
                                    continue;
                                }
                                if !search.is_empty()
                                    && !action.name.to_lowercase().contains(&search)
                                {
                                    continue;
                                }

                                if ui.button(&action.name).clicked() {
                                    let ctx_to_set = shared::Context {
                                        device: device.id.clone(),
                                        profile: selected_profile.clone(),
                                        controller: slot.controller.clone(),
                                        position: slot.position,
                                    };
                                    let _ = self.runtime.block_on(async {
                                        riverdeck_core::api::instances::create_instance(
                                            action, ctx_to_set,
                                        )
                                        .await
                                    });
                                }
                            }
                        });
                }
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            let Some(device) = &selected_device else {
                ui.label("Select a device to view details.");
                return;
            };

            ui.heading(&device.name);
            ui.monospace(&device.id);

            ui.separator();

            ui.label(format!("Selected profile: {selected_profile}"));
            if ui.button("Reload plugins").clicked() {
                plugins::initialise_plugins();
            }

            if ui
                .button("Open property inspector (first action w/ PI)")
                .clicked()
            {
                let result = self.open_first_property_inspector(&device.id);
                if let Err(err) = result {
                    log::warn!("Failed to open property inspector: {err}");
                }
            }

            ui.separator();
            ui.heading("Device preview");

            let Some(snapshot) = &snapshot else {
                ui.label("Loading profile…");
                return;
            };

            let key_size = egui::vec2(72.0, 72.0);
            let cols = device.columns as usize;
            let rows = device.rows as usize;
            for r in 0..rows {
                ui.horizontal(|ui| {
                    for c in 0..cols {
                        let pos = (r * cols + c) as u8;
                        let slot = SelectedSlot {
                            controller: "Keypad".to_owned(),
                            position: pos,
                        };
                        let instance = snapshot.keys.get(pos as usize).and_then(|v| v.as_ref());
                        let selected = self.selected_slot.as_ref() == Some(&slot);
                        let resp =
                            self.draw_slot_preview(ui, ctx, key_size, &slot, instance, selected);
                        if resp.clicked() {
                            self.selected_slot = Some(slot);
                        }
                    }
                });
            }

            if device.encoders > 0 {
                ui.separator();
                ui.heading("Encoders");
                ui.horizontal_wrapped(|ui| {
                    for i in 0..(device.encoders as usize) {
                        let slot = SelectedSlot {
                            controller: "Encoder".to_owned(),
                            position: i as u8,
                        };
                        let instance = snapshot.sliders.get(i).and_then(|v| v.as_ref());
                        let selected = self.selected_slot.as_ref() == Some(&slot);
                        let resp =
                            self.draw_slot_preview(ui, ctx, key_size, &slot, instance, selected);
                        if resp.clicked() {
                            self.selected_slot = Some(slot);
                        }
                    }
                });
            }
        });

        ctx.request_repaint_after(std::time::Duration::from_millis(100));
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
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                // Only this label acts as the drag handle, so clicks on the window buttons work reliably.
                let drag = ui.add(
                    egui::Label::new(egui::RichText::new("RiverDeck").strong())
                        .sense(egui::Sense::click_and_drag()),
                );
                if drag.drag_started() || drag.dragged() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                ui.label(format!("devices: {}", shared::DEVICES.len()));

                if let Some(info) = self.update_info.lock().unwrap().as_ref() {
                    ui.separator();
                    ui.label(format!("Update available: {}", info.tag));
                    if ui.button("Details").clicked() {
                        self.show_update_details = true;
                    }
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;

                    let close = ui.button("✕");
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
                    if ui.button(max_label).clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                    }

                    if ui.button("—").clicked() {
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
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
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
    // Embed the icon so tray support doesn't depend on the current working directory.
    let bytes = include_bytes!("../../../packaging/icons/icon.png");
    let img = image::load_from_memory(bytes)?.into_rgba8();
    let (w, h) = (img.width(), img.height());
    Ok(Icon::from_rgba(img.into_raw(), w, h)?)
}

impl RiverDeckApp {
    fn open_first_property_inspector(&self, device_id: &str) -> anyhow::Result<()> {
        let (instance, port_base) = self.runtime.block_on(async {
            let mut locks = riverdeck_core::store::profiles::acquire_locks_mut().await;
            let selected_profile = locks.device_stores.get_selected_profile(device_id)?;
            let device = shared::DEVICES
                .get(device_id)
                .ok_or_else(|| anyhow::anyhow!("device not found"))?;
            let store = locks
                .profile_stores
                .get_profile_store(&device, &selected_profile)?;
            let instance = store
                .value
                .keys
                .iter()
                .chain(store.value.sliders.iter())
                .flatten()
                .find(|i| !i.action.property_inspector.trim().is_empty())
                .cloned();
            anyhow::Ok((instance, *riverdeck_core::plugins::PORT_BASE))
        })?;

        let Some(instance) = instance else {
            return Err(anyhow::anyhow!(
                "no action with a property inspector found in selected profile"
            ));
        };

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

        let ctx_str = instance.context.to_string();
        let split: Vec<&str> = ctx_str.split('.').collect();
        let position: u8 = split.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
        let controller = split.get(2).copied().unwrap_or("Keypad");

        let coordinates = if controller == "Encoder" {
            serde_json::json!({ "row": 0, "column": position })
        } else {
            let device = shared::DEVICES
                .get(device_id)
                .ok_or_else(|| anyhow::anyhow!("device not found"))?;
            serde_json::json!({ "row": (position / device.columns), "column": (position % device.columns) })
        };

        let connect_payload = serde_json::json!({
            "action": instance.action.uuid,
            "context": instance.context.to_string(),
            "device": split.first().copied().unwrap_or(device_id),
            "payload": {
                "settings": instance.settings,
                "coordinates": coordinates,
                "controller": controller,
                "state": instance.current_state,
                "isInMultiAction": instance.context.index != 0,
            }
        });

        let origin = format!("http://localhost:{}", port_base + 2);
        let pi_src = format!(
            "{origin}/{}|riverdeck_property_inspector",
            instance.action.property_inspector
        );

        let label = format!("pi_{}", instance.context.to_string().replace('.', "_"));
        let info_json = serde_json::to_string(&info)?;
        let connect_json = serde_json::to_string(&connect_payload)?;
        spawn_pi_process(
            &label,
            &pi_src,
            &origin,
            port_base,
            &instance.context.to_string(),
            &info_json,
            &connect_json,
        )?;

        self.runtime.block_on(async {
			let _ = riverdeck_core::events::outbound::property_inspector::property_inspector_did_appear(instance.context.clone(), "propertyInspectorDidAppear").await;
		});

        Ok(())
    }
}

fn spawn_pi_process(
    label: &str,
    pi_src: &str,
    origin: &str,
    port: u16,
    context: &str,
    info_json: &str,
    connect_json: &str,
) -> anyhow::Result<()> {
    let mut exe = std::env::current_exe()?;
    exe.set_file_name(if cfg!(windows) {
        "riverdeck-pi.exe"
    } else {
        "riverdeck-pi"
    });

    std::process::Command::new(exe)
        .arg("--label")
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
        .arg(connect_json)
        .spawn()?;

    Ok(())
}
