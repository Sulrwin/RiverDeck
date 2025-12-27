pub mod info_param;
pub mod manifest;
mod webserver;

use crate::shared::{CATEGORIES, Category, config_dir, convert_icon, is_flatpak, log_dir};
use crate::store::get_settings;
use crate::ui::{self, UiEvent};
use crate::webview;

use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::{fs, path};

use base64::Engine as _;
use futures::StreamExt;
use tokio::net::{TcpListener, TcpStream};

use anyhow::anyhow;
use log::{error, warn};
use once_cell::sync::Lazy;
use tokio::sync::{Mutex, RwLock};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

fn is_safe_relative_path(p: &str) -> bool {
    let p = std::path::Path::new(p);
    !p.is_absolute()
        && !p.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
}

enum PluginInstance {
    Webview { label: String },
    Wine(Child),
    Native(Child),
    Node(Child),
}

pub static DEVICE_NAMESPACES: Lazy<RwLock<HashMap<String, String>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
static INSTANCES: Lazy<Mutex<HashMap<String, PluginInstance>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static PLUGIN_SERVERS_STARTED: Lazy<AtomicBool> = Lazy::new(|| AtomicBool::new(false));

static PROPERTY_INSPECTOR_TOKEN: Lazy<String> = Lazy::new(|| {
    // Random per-process token used to make it harder for other local users to impersonate
    // property inspectors (best-effort; not a sandbox).
    let mut bytes = [0u8; 32];
    if getrandom::getrandom(&mut bytes).is_err() {
        // Fallback: still produce something, but log that we couldn't get strong randomness.
        log::warn!("getrandom failed; property inspector auth token is weak");
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        bytes[..16].copy_from_slice(&t.to_le_bytes());
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
});

pub fn property_inspector_token() -> &'static str {
    PROPERTY_INSPECTOR_TOKEN.as_str()
}

pub static PORT_BASE: Lazy<u16> = Lazy::new(|| {
    let mut base = 57116;
    loop {
        // These servers should only ever be reachable locally. Binding to 0.0.0.0 would expose
        // the plugin control plane on the LAN.
        let websocket_result = std::net::TcpListener::bind(("127.0.0.1", base));
        let webserver_result = std::net::TcpListener::bind(("127.0.0.1", base + 2));
        if websocket_result.is_ok() && webserver_result.is_ok() {
            log::debug!("Using ports {} and {}", base, base + 2);
            break;
        }
        base += 1;
    }
    base
});

/// Initialise a plugin from a given directory.
pub async fn initialise_plugin(path: &path::Path) -> anyhow::Result<()> {
    let plugin_uuid = path.file_name().unwrap().to_str().unwrap();
    let target = std::env::var("TARGET").unwrap_or_default();

    let mut manifest = manifest::read_manifest(path)?;

    // RiverDeck branding: treat legacy OpenDeck categories as our built-in category.
    // This covers plugins installed under the user's config dir that still declare `"Category": "OpenDeck"`.
    if manifest.category.trim() == "OpenDeck" {
        manifest.category = crate::shared::PRODUCT_NAME.to_owned();
    }
    if manifest.name.trim() == "OpenDeck Starter Pack" {
        manifest.name = "RiverDeck Starter Pack".to_owned();
    }

    if let Some(icon) = manifest.category_icon {
        let category_icon_path = path.join(icon);
        manifest.category_icon = Some(convert_icon(
            category_icon_path.to_string_lossy().to_string(),
        ));
    }

    for action in &mut manifest.actions {
        plugin_uuid.clone_into(&mut action.plugin);

        let action_icon_path = path.join(action.icon.clone());
        action.icon = convert_icon(action_icon_path.to_str().unwrap().to_owned());

        if !action.property_inspector.is_empty() {
            if is_safe_relative_path(&action.property_inspector) {
                action.property_inspector = path
                    .join(&action.property_inspector)
                    .to_string_lossy()
                    .to_string();
            } else {
                warn!(
                    "Plugin {} has unsafe PropertyInspectorPath {}; ignoring",
                    plugin_uuid, action.property_inspector
                );
                action.property_inspector.clear();
            }
        } else if let Some(ref property_inspector) = manifest.property_inspector_path
            && is_safe_relative_path(property_inspector)
        {
            action.property_inspector = path.join(property_inspector).to_string_lossy().to_string();
        }

        for state in &mut action.states {
            if state.image == "actionDefaultImage" {
                state.image.clone_from(&action.icon);
            } else {
                let state_icon = path.join(state.image.clone());
                state.image = convert_icon(state_icon.to_str().unwrap().to_owned());
            }

            match state.family.clone().to_lowercase().trim() {
                "arial" => "Liberation Sans",
                "arial black" => "Archivo Black",
                "comic sans ms" => "Comic Neue",
                "courier" | "Courier New" => "Courier Prime",
                "georgia" => "Tinos",
                "impact" => "Anton",
                "microsoft sans serif" | "Times New Roman" => "Liberation Serif",
                "tahoma" | "Verdana" => "Open Sans",
                "trebuchet ms" => "Fira Sans",
                _ => continue,
            }
            .clone_into(&mut state.family);
        }
    }

    {
        let mut categories = CATEGORIES.write().await;
        if let Some(category) = categories.get_mut(&manifest.category) {
            for action in manifest.actions {
                if let Some(index) = category.actions.iter().position(|v| v.uuid == action.uuid) {
                    category.actions.remove(index);
                }
                category.actions.push(action);
            }
        } else {
            let mut category: Category = Category {
                icon: manifest.category_icon,
                actions: vec![],
            };
            for action in manifest.actions {
                category.actions.push(action);
            }
            if !category.actions.is_empty() {
                categories.insert(manifest.category, category);
            }
        }
    }

    if let Some(namespace) = manifest.device_namespace {
        DEVICE_NAMESPACES
            .write()
            .await
            .insert(namespace, plugin_uuid.to_owned());
    }

    #[cfg(target_os = "windows")]
    let platform = "windows";
    #[cfg(target_os = "macos")]
    let platform = "mac";
    #[cfg(target_os = "linux")]
    let platform = "linux";

    let mut code_path = manifest.code_path;
    let mut use_wine = false;
    let mut supported = false;

    // Determine the method used to run the plugin based on its supported operating systems and the current operating system.
    for os in manifest.os {
        if os.platform == platform {
            #[cfg(target_os = "windows")]
            if manifest.code_path_windows.is_some() {
                code_path = manifest.code_path_windows.clone();
            }
            #[cfg(target_os = "macos")]
            if manifest.code_path_macos.is_some() {
                code_path = manifest.code_path_macos;
            }
            #[cfg(target_os = "linux")]
            if manifest.code_path_linux.is_some() {
                code_path = manifest.code_path_linux;
            }
            if !target.is_empty() {
                code_path = manifest
                    .code_paths
                    .as_ref()
                    .and_then(|p| p.get(&target).cloned())
                    .or(code_path);
            }

            use_wine = false;

            supported = true;
            break;
        } else if os.platform == "windows" {
            use_wine = true;
            supported = true;
        }
    }

    if code_path.is_none() && use_wine {
        code_path = manifest.code_path_windows;
    }

    if !supported || code_path.is_none() {
        return Err(anyhow!("unsupported on platform {}", platform));
    }

    let code_path = code_path.unwrap();
    if !is_safe_relative_path(&code_path) {
        return Err(anyhow!("unsafe plugin CodePath"));
    }
    let port_string = PORT_BASE.to_string();
    let args = [
        "-port",
        port_string.as_str(),
        "-pluginUUID",
        plugin_uuid,
        "-registerEvent",
        "registerPlugin",
        "-info",
    ];

    if code_path.to_lowercase().ends_with(".html")
        || code_path.to_lowercase().ends_with(".htm")
        || code_path.to_lowercase().ends_with(".xhtml")
    {
        let url = format!("http://localhost:{}/", *PORT_BASE + 2)
            + path.join(&code_path).to_str().unwrap();
        let label = plugin_uuid.replace('.', "_");

        let Some(host) = webview::webview_host() else {
            return Err(anyhow!(
                "HTML plugin {plugin_uuid} requires a WebviewHost, but none is configured"
            ));
        };

        let info = info_param::make_info(plugin_uuid.to_owned(), manifest.version, false).await;
        let init_js = format!(
            r#"const opendeckInit = () => {{
				try {{
					if (typeof connectOpenActionSocket === "function") connectOpenActionSocket({port}, "{uuid}", "{event}", `{info}`);
					else connectElgatoStreamDeckSocket({port}, "{uuid}", "{event}", `{info}`);
				}} catch (e) {{
					setTimeout(opendeckInit, 10);
				}}
			}};
			opendeckInit();
			"#,
            port = *PORT_BASE,
            uuid = plugin_uuid,
            event = "registerPlugin",
            info = serde_json::to_string(&info)?
        );

        host.spawn_plugin_webview(&label, &url, &init_js)?;

        if let Ok(store) = get_settings()
            && store.value.developer
        {
            let _ = host.show_webview(&label);
            let _ = host.open_devtools(&label);
        }

        INSTANCES
            .lock()
            .await
            .insert(plugin_uuid.to_owned(), PluginInstance::Webview { label });
    } else if code_path.to_lowercase().ends_with(".js")
        || code_path.to_lowercase().ends_with(".mjs")
        || code_path.to_lowercase().ends_with(".cjs")
    {
        // Check for Node.js installation and version in one go.
        let command = if is_flatpak() {
            "flatpak-spawn"
        } else {
            "node"
        };
        let extra_args = if is_flatpak() {
            vec!["--host", "node"]
        } else {
            vec![]
        };
        let version_output = Command::new(command)
            .args(&extra_args)
            .arg("--version")
            .output();
        let version_ok = version_output
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().trim_start_matches('v').to_owned())
            .and_then(|s| semver::Version::parse(&s).ok())
            .is_some_and(|v| v >= semver::Version::new(20, 0, 0));
        if !version_ok {
            return Err(anyhow!("Node.js version 20.0.0 or higher is required"));
        }

        let info = info_param::make_info(plugin_uuid.to_owned(), manifest.version, true).await;
        let log_file =
            fs::File::create(log_dir().join("plugins").join(format!("{plugin_uuid}.log")))?;

        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            let child = Command::new(command)
                .current_dir(path)
                .args(extra_args)
                .arg(code_path)
                .args(args)
                .arg(serde_json::to_string(&info)?)
                .stdin(Stdio::null())
                .stdout(Stdio::from(log_file.try_clone()?))
                .stderr(Stdio::from(log_file))
                .creation_flags(0x08000000)
                .spawn()?;

            INSTANCES
                .lock()
                .await
                .insert(plugin_uuid.to_owned(), PluginInstance::Node(child));
        }

        #[cfg(not(target_os = "windows"))]
        {
            let child = Command::new(command)
                .current_dir(path)
                .args(extra_args)
                .arg(code_path)
                .args(args)
                .arg(serde_json::to_string(&info)?)
                .stdin(Stdio::null())
                .stdout(Stdio::from(log_file.try_clone()?))
                .stderr(Stdio::from(log_file))
                .spawn()?;

            INSTANCES
                .lock()
                .await
                .insert(plugin_uuid.to_owned(), PluginInstance::Node(child));
        }
    } else if use_wine {
        let command = if is_flatpak() {
            "flatpak-spawn"
        } else {
            "wine"
        };
        let extra_args = if is_flatpak() {
            vec!["--host", "wine"]
        } else {
            vec![]
        };
        let result = Command::new(command)
            .args(&extra_args)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .and_then(|mut child| child.wait())
            .map(|status| status.success());
        if !matches!(result, Ok(true)) {
            return Err(anyhow!("failed to detect an installation of Wine"));
        }

        let info = info_param::make_info(plugin_uuid.to_owned(), manifest.version, true).await;
        let log_file =
            fs::File::create(log_dir().join("plugins").join(format!("{plugin_uuid}.log")))?;

        let mut command = Command::new(command);
        command
            .current_dir(path)
            .args(extra_args)
            .arg(code_path)
            .args(args)
            .arg(serde_json::to_string(&info)?)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_file.try_clone()?))
            .stderr(Stdio::from(log_file));
        if get_settings()?.value.separatewine {
            command.env(
                "WINEPREFIX",
                path.join("wineprefix").to_string_lossy().to_string(),
            );
        } else {
            let _ = fs::remove_dir_all(path.join("wineprefix"));
        }
        let child = command.spawn()?;

        INSTANCES
            .lock()
            .await
            .insert(plugin_uuid.to_owned(), PluginInstance::Wine(child));
    } else {
        let info = info_param::make_info(plugin_uuid.to_owned(), manifest.version, false).await;
        let log_file =
            fs::File::create(log_dir().join("plugins").join(format!("{plugin_uuid}.log")))?;

        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            let child = Command::new(path.join(code_path))
                .current_dir(path)
                .args(args)
                .arg(serde_json::to_string(&info)?)
                .stdin(Stdio::null())
                .stdout(Stdio::from(log_file.try_clone()?))
                .stderr(Stdio::from(log_file))
                .creation_flags(0x08000000)
                .spawn()?;

            INSTANCES
                .lock()
                .await
                .insert(plugin_uuid.to_owned(), PluginInstance::Native(child));
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path.join(&code_path), fs::Permissions::from_mode(0o755))?;
        }

        #[cfg(not(target_os = "windows"))]
        {
            let child = Command::new(path.join(code_path))
                .current_dir(path)
                .args(args)
                .arg(serde_json::to_string(&info)?)
                .stdin(Stdio::null())
                .stdout(Stdio::from(log_file.try_clone()?))
                .stderr(Stdio::from(log_file))
                .spawn()?;

            INSTANCES
                .lock()
                .await
                .insert(plugin_uuid.to_owned(), PluginInstance::Native(child));
        }
    }

    if let Some(applications) = manifest.applications_to_monitor
        && let Some(applications) = applications.get(platform)
    {
        crate::application_watcher::start_monitoring(plugin_uuid, applications).await;
    }

    Ok(())
}

pub async fn deactivate_plugin(uuid: &str) -> Result<(), anyhow::Error> {
    {
        let mut namespaces = DEVICE_NAMESPACES.write().await;
        if let Some((namespace, _)) = namespaces
            .clone()
            .iter()
            .find(|(_, plugin)| uuid == **plugin)
        {
            namespaces.remove(namespace);
            drop(namespaces);
            let devices = crate::shared::DEVICES
                .iter()
                .map(|v| v.key().to_owned())
                .filter(|id| &id[..2] == namespace)
                .collect::<Vec<_>>();
            for device in devices {
                crate::events::inbound::devices::deregister_device(
                    "",
                    crate::events::inbound::PayloadEvent { payload: device },
                )
                .await?;
            }
            ui::emit(UiEvent::DevicesUpdated);
        }
    }

    crate::application_watcher::stop_monitoring(uuid).await;

    if let Some(instance) = INSTANCES.lock().await.remove(uuid) {
        match instance {
            PluginInstance::Webview { label } => {
                if let Some(host) = webview::webview_host() {
                    let _ = host.close_webview(&label);
                }
            }
            PluginInstance::Node(mut child)
            | PluginInstance::Wine(mut child)
            | PluginInstance::Native(mut child) => {
                child.kill()?;
                child.wait()?;
            }
        }
        Ok(())
    } else {
        Err(anyhow!("instance of plugin {} not found", uuid))
    }
}

#[cfg(windows)]
pub async fn deactivate_plugins() {
    let uuids = {
        let instances = INSTANCES.lock().await;
        instances.keys().cloned().collect::<Vec<_>>()
    };

    for uuid in uuids {
        let _ = deactivate_plugin(&uuid).await;
    }
}

/// Initialise plugins from the plugins directory.
pub fn initialise_plugins() {
    // Servers should be started only once; reloading plugins should not rebind ports.
    if !PLUGIN_SERVERS_STARTED.swap(true, Ordering::SeqCst) {
        tokio::spawn(init_websocket_server());
        tokio::spawn(webserver::init_webserver(config_dir()));
    } else {
        log::debug!("Plugin servers already running; skipping server init");
    }

    let plugin_dir = config_dir().join("plugins");
    let _ = fs::create_dir_all(&plugin_dir);
    let _ = fs::create_dir_all(log_dir().join("plugins"));

    // Remove any legacy OpenDeck category bucket so reloading doesn't keep showing it.
    // (Reload will re-add actions under `PRODUCT_NAME` via the normalization in `initialise_plugin`.)
    tokio::spawn(async {
        let mut cats = crate::shared::CATEGORIES.write().await;
        if let Some(mut open) = cats.remove("OpenDeck") {
            let target = cats
                .entry(crate::shared::PRODUCT_NAME.to_owned())
                .or_insert(crate::shared::Category {
                    icon: None,
                    actions: vec![],
                });
            target.actions.append(&mut open.actions);
        }
    });

    if let Some(resource_dir) = crate::shared::resource_dir() {
        let builtin_plugins_dir = resource_dir.join("plugins");
        if let Ok(entries) = fs::read_dir(&builtin_plugins_dir) {
            for entry in entries.flatten() {
                if let Err(error) = (|| -> Result<(), anyhow::Error> {
                    let builtin_version = semver::Version::parse(
                        &serde_json::from_slice::<manifest::PluginManifest>(&fs::read(
                            entry.path().join("manifest.json"),
                        )?)?
                        .version,
                    )?;
                    let existing_path = plugin_dir.join(entry.file_name());
                    if (|| -> Result<(), anyhow::Error> {
                        let existing_version = semver::Version::parse(
                            &serde_json::from_slice::<manifest::PluginManifest>(&fs::read(
                                existing_path.join("manifest.json"),
                            )?)?
                            .version,
                        )?;
                        if existing_version < builtin_version {
                            Err(anyhow::anyhow!(
                                "builtin version is newer than existing version"
                            ))
                        } else {
                            Ok(())
                        }
                    })()
                    .is_err()
                    {
                        if existing_path.exists() {
                            fs::rename(&existing_path, existing_path.with_extension("old"))?;
                        }
                        if crate::shared::copy_dir(entry.path(), &existing_path).is_err()
                            && existing_path.with_extension("old").exists()
                        {
                            fs::rename(existing_path.with_extension("old"), &existing_path)?;
                        }
                        let _ = fs::remove_dir_all(existing_path.with_extension("old"));
                    }
                    Ok(())
                })() {
                    error!(
                        "Failed to upgrade builtin plugin {}: {}",
                        entry.file_name().to_string_lossy(),
                        error
                    );
                }
            }
        }
    }

    let entries = match fs::read_dir(&plugin_dir) {
        Ok(p) => p,
        Err(error) => {
            error!(
                "Failed to read plugins directory at {}: {}",
                plugin_dir.display(),
                error
            );
            panic!()
        }
    };

    // Iterate through all directory entries in the plugins folder and initialise them as plugins if appropriate
    for entry in entries {
        if let Ok(entry) = entry {
            let entry_path = entry.path();
            let meta = match fs::symlink_metadata(&entry_path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let path = if meta.file_type().is_symlink() {
                // Only follow symlinks if they stay within the plugins directory (avoid surprising escapes).
                let target = match fs::read_link(&entry_path) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                let plugins_canon = match fs::canonicalize(&plugin_dir) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                let target_canon = match fs::canonicalize(&target) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                if !target_canon.starts_with(&plugins_canon) {
                    warn!(
                        "Ignoring plugin symlink {} -> {} (escapes plugins dir)",
                        entry_path.display(),
                        target.display()
                    );
                    continue;
                }
                target
            } else {
                entry_path
            };
            let metadata = match fs::metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if metadata.is_dir() {
                tokio::spawn(async move {
                    if let Err(error) = initialise_plugin(&path).await {
                        warn!(
                            "Failed to initialise plugin at {}: {:#}",
                            path.display(),
                            error
                        );
                    }
                });
            }
        } else if let Err(error) = entry {
            warn!("Failed to read entry of plugins directory: {}", error)
        }
    }
}

/// Start the WebSocket server that plugins communicate with.
async fn init_websocket_server() {
    let listener = match TcpListener::bind(("127.0.0.1", *PORT_BASE)).await {
        Ok(listener) => listener,
        Err(error) => {
            error!(
                "Failed to bind plugin WebSocket server to socket: {}",
                error
            );
            return;
        }
    };

    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};

        unsafe { SetHandleInformation(listener.as_raw_socket() as _, HANDLE_FLAG_INHERIT, 0) };
    }

    while let Ok((stream, _)) = listener.accept().await {
        accept_connection(stream).await;
    }
}

/// Handle incoming data from a WebSocket connection.
async fn accept_connection(stream: TcpStream) {
    // Put a hard cap on message sizes to mitigate trivial memory/CPU DoS.
    // `setImage` can include a data URI, so keep this reasonably sized.
    const MAX_WS_MESSAGE_BYTES: usize = 10 * 1024 * 1024;
    let mut cfg = WebSocketConfig::default();
    cfg.max_message_size = Some(MAX_WS_MESSAGE_BYTES);
    cfg.max_frame_size = Some(MAX_WS_MESSAGE_BYTES);
    cfg.accept_unmasked_frames = false;

    let mut socket = match tokio_tungstenite::accept_async_with_config(stream, Some(cfg)).await {
        Ok(socket) => socket,
        Err(error) => {
            warn!("Failed to complete WebSocket handshake: {}", error);
            return;
        }
    };

    // First message must be a registration event. Never `unwrap()` here: a client can disconnect
    // immediately, and a panic would take down the whole server task.
    let Some(first) = socket.next().await else {
        return;
    };
    let first = match first {
        Ok(m) => m,
        Err(e) => {
            warn!("WebSocket error before registration: {}", e);
            return;
        }
    };
    let text = match first.into_text() {
        Ok(t) => t,
        Err(_) => return,
    };

    match serde_json::from_str::<crate::events::inbound::RegisterEvent>(&text) {
        Ok(event) => crate::events::register_plugin(event, socket).await,
        Err(_) => {
            let _ = crate::events::inbound::process_incoming_message(
                Ok(tokio_tungstenite::tungstenite::Message::Text(text)),
                "",
                false,
            )
            .await;
        }
    };
}

#[cfg(test)]
mod tests {
    use super::is_safe_relative_path;

    #[test]
    fn safe_relative_path_rejects_traversal_and_absolute() {
        assert!(is_safe_relative_path("foo/bar.exe"));
        assert!(is_safe_relative_path("assets/propertyInspector/index.html"));

        assert!(!is_safe_relative_path("../evil"));
        assert!(!is_safe_relative_path("foo/../../evil"));
        assert!(!is_safe_relative_path("/etc/passwd"));
    }
}
