use riverdeck_core::{application_watcher, elgato, plugins, shared, ui};

use std::sync::Arc;
use std::{fs, fs::OpenOptions, path::Path, sync::Mutex};

use fs2::FileExt;
use tokio::runtime::Runtime;
use tokio::sync::broadcast;

#[cfg(feature = "tray")]
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
#[cfg(feature = "tray")]
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

fn main() -> anyhow::Result<()> {
    env_logger::init();

    let args: Vec<String> = std::env::args().collect();
    let start_hidden = args.iter().any(|a| a == "--hide");

    let mut paths = shared::discover_paths()?;
    // Best-effort: in dev, allow bundled plugins from repo root `plugins/`.
    // In packaged builds, callers should set this to the actual resource directory.
    let dev_resource_dir = std::env::current_dir().ok();
    if dev_resource_dir
        .as_ref()
        .is_some_and(|d| looks_like_bundled_resources(d))
    {
        paths.resource_dir = dev_resource_dir;
    }
    shared::init_paths(paths);

    configure_autostart();

    // Single-instance: best-effort lockfile. (IPC handoff is added later.)
    std::fs::create_dir_all(shared::config_dir())?;
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(shared::config_dir().join("riverdeck.lock"))?;
    if lock_file.try_lock_exclusive().is_err() {
        // Another instance is running.
        return Ok(());
    }

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

    let native_options = eframe::NativeOptions::default();
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
    selected_device: Option<String>,

    show_update_details: bool,
}

impl RiverDeckApp {
    fn new(
        runtime: Arc<Runtime>,
        lock_file: std::fs::File,
        start_hidden: bool,
        update_info: Arc<Mutex<Option<UpdateInfo>>>,
    ) -> Self {
        Self {
            runtime,
            ui_events: ui::subscribe(),
            _lock_file: lock_file,
            start_hidden,
            update_info,
            #[cfg(feature = "tray")]
            tray: TrayState::new().ok(),
            selected_device: None,
            show_update_details: false,
        }
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
        if self.start_hidden {
            self.start_hidden = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        #[cfg(feature = "tray")]
        if let Some(tray) = &mut self.tray {
            tray.poll(ctx);
        }

        self.poll_ui_events();

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
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
                }
            }
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            let Some(device_id) = self.selected_device.clone() else {
                ui.label("Select a device to view details.");
                return;
            };
            let Some(device) = shared::DEVICES.get(&device_id) else {
                ui.label("Selected device is no longer connected.");
                self.selected_device = None;
                return;
            };

            ui.heading(&device.name);
            ui.monospace(&device.id);

            ui.separator();

            let selected_profile = self
                .runtime
                .block_on(async {
                    riverdeck_core::api::profiles::get_selected_profile(device.id.clone())
                        .await
                        .map(|p| p.id)
                })
                .unwrap_or_else(|_| "Default".to_owned());

            ui.label(format!("Selected profile: {selected_profile}"));
            if ui.button("Reload plugins").clicked() {
                plugins::initialise_plugins();
            }

            if ui
                .button("Open property inspector (first action w/ PI)")
                .clicked()
            {
                let device_id = device.id.clone();
                let result = self.open_first_property_inspector(&device_id);
                if let Err(err) = result {
                    log::warn!("Failed to open property inspector: {err}");
                }
            }
        });

        ctx.request_repaint_after(std::time::Duration::from_millis(100));
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
            } else if ev.id == self.hide.id() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            } else if ev.id == self.quit.id() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
}

#[cfg(feature = "tray")]
fn load_tray_icon() -> anyhow::Result<Icon> {
    let path = std::env::current_dir()?
        .join("src-tauri")
        .join("icons")
        .join("icon.png");
    let img = image::open(&path)?.into_rgba8();
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
