// Prevents additional console window on Windows in release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod application_watcher;
mod elgato;
mod events;
mod plugins;
mod shared;
mod store;
mod zip_extract;

mod built_info {
	include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

use events::frontend;
use shared::PRODUCT_NAME;

use once_cell::sync::OnceCell;
use tauri::{
	AppHandle, Builder, Manager, WindowEvent,
	menu::{IconMenuItemBuilder, MenuBuilder, MenuItemBuilder, PredefinedMenuItem},
	tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};
use tauri_plugin_log::{Target, TargetKind};

static APP_HANDLE: OnceCell<AppHandle> = OnceCell::new();

fn is_empty_dir(path: &std::path::Path) -> bool {
	match std::fs::read_dir(path) {
		Ok(mut it) => it.next().is_none(),
		Err(_) => true,
	}
}

fn rewrite_opendeck_profile_json(value: &mut serde_json::Value) -> bool {
	let mut changed = false;
	match value {
		serde_json::Value::Object(map) => {
			for (k, v) in map.iter_mut() {
				if k == "plugin" {
					if let serde_json::Value::String(s) = v {
						if s == "opendeck" {
							*s = "riverdeck".to_owned();
							changed = true;
						}
					}
				} else if k == "uuid" {
					if let serde_json::Value::String(s) = v {
						if s == "opendeck.multiaction" {
							*s = "riverdeck.multiaction".to_owned();
							changed = true;
						} else if s == "opendeck.toggleaction" {
							*s = "riverdeck.toggleaction".to_owned();
							changed = true;
						}
					}
				}

				changed |= rewrite_opendeck_profile_json(v);
			}
		}
		serde_json::Value::Array(arr) => {
			for v in arr.iter_mut() {
				changed |= rewrite_opendeck_profile_json(v);
			}
		}
		serde_json::Value::String(s) => {
			if let Some(rest) = s.strip_prefix("opendeck/") {
				*s = format!("riverdeck/{rest}");
				changed = true;
			}
		}
		_ => {}
	}
	changed
}

fn rewrite_profiles_on_disk(profiles_dir: &std::path::Path) -> Result<(), std::io::Error> {
	fn visit_dir(dir: &std::path::Path) -> Result<Vec<std::path::PathBuf>, std::io::Error> {
		let mut files = vec![];
		for entry in std::fs::read_dir(dir)? {
			let entry = entry?;
			let path = entry.path();
			if entry.file_type()?.is_dir() {
				files.extend(visit_dir(&path)?);
			} else {
				files.push(path);
			}
		}
		Ok(files)
	}

	if !profiles_dir.exists() {
		return Ok(());
	}

	for path in visit_dir(profiles_dir)? {
		let name = path.file_name().and_then(|v| v.to_str()).unwrap_or("");
		if !(name.ends_with(".json") || name.ends_with(".json.bak") || name.ends_with(".json.temp")) {
			continue;
		}
		let Ok(contents) = std::fs::read_to_string(&path) else { continue };
		let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&contents) else { continue };
		if rewrite_opendeck_profile_json(&mut json) {
			// Best-effort: preserve readability, but don't fail migration if this fails.
			if let Ok(updated) = serde_json::to_string_pretty(&json) {
				let _ = std::fs::write(&path, updated);
			}
		}
	}
	Ok(())
}

fn migrate_opendeck_to_riverdeck(app: &tauri::App) {
	let Ok(new_config_dir) = app.path().app_config_dir() else { return };
	let Ok(new_log_dir) = app.path().app_log_dir() else { return };

	let marker = new_config_dir.join("migrated_from_opendeck");
	if marker.exists() {
		return;
	}

	let mut migrated_anything = false;

	let config_base = app.path().config_dir().ok();
	let data_base = app.path().data_dir().ok();
	let home = std::env::var_os("HOME").map(std::path::PathBuf::from);

	let mut old_candidates: Vec<std::path::PathBuf> = vec![];
	if let Some(config_base) = &config_base {
		old_candidates.extend([
			config_base.join("opendeck"),
			config_base.join("com.amansprojects.opendeck"),
			config_base.join("me.amankhanna.opendeck"),
		]);
	}
	// Legacy Linux XDG hardcoded paths (in case config_base isn't available or differs)
	if let Some(home) = &home {
		old_candidates.extend([
			home.join(".config/opendeck"),
			home.join(".config/com.amansprojects.opendeck"),
			home.join(".config/me.amankhanna.opendeck"),
		]);
	}

	let old_config_dir = old_candidates.into_iter().find(|p| p.exists());

	// Copy config first (profiles, settings, plugins, etc)
	if let Some(old_config_dir) = old_config_dir {
		let _ = std::fs::create_dir_all(&new_config_dir);
		if is_empty_dir(&new_config_dir) {
			if let Err(err) = crate::shared::copy_dir(&old_config_dir, &new_config_dir) {
				log::warn!("Failed to migrate config dir from {:?} to {:?}: {err}", old_config_dir, new_config_dir);
			} else {
				log::info!("Migrated config dir from {:?} to {:?}", old_config_dir, new_config_dir);
				migrated_anything = true;
			}
		}
	}

	// Copy logs/data if present (best-effort)
	let mut old_log_candidates: Vec<std::path::PathBuf> = vec![];
	if let Some(data_base) = &data_base {
		old_log_candidates.extend([data_base.join("opendeck").join("logs"), data_base.join("opendeck")]);
	}
	if let Some(home) = &home {
		old_log_candidates.extend([home.join(".local/share/opendeck/logs"), home.join(".local/share/opendeck")]);
	}

	if let Some(old_log_dir) = old_log_candidates.into_iter().find(|p| p.exists()) {
		let _ = std::fs::create_dir_all(&new_log_dir);
		if is_empty_dir(&new_log_dir) {
			if let Err(err) = crate::shared::copy_dir(&old_log_dir, &new_log_dir) {
				log::warn!("Failed to migrate log dir from {:?} to {:?}: {err}", old_log_dir, new_log_dir);
			} else {
				log::info!("Migrated log dir from {:?} to {:?}", old_log_dir, new_log_dir);
				migrated_anything = true;
			}
		}
	}

	// Mark migration as done (even if partial), to avoid repeatedly copying.
	if migrated_anything {
		// Rewrite migrated profiles so RiverDeck's built-in actions still work.
		let _ = rewrite_profiles_on_disk(&new_config_dir.join("profiles"));

		let _ = std::fs::create_dir_all(&new_config_dir);
		let _ = std::fs::write(&marker, b"ok\n");
	}
}

fn show_window(app: &AppHandle) -> Result<(), tauri::Error> {
	#[cfg(target_os = "macos")]
	{
		use tauri::ActivationPolicy;
		let _ = app.set_activation_policy(ActivationPolicy::Regular);
	}

	let window = app.get_webview_window("main").ok_or_else(|| tauri::Error::WebviewNotFound)?;
	window.show().and_then(|_| window.set_focus())
}

fn hide_window(app: &AppHandle) -> Result<(), tauri::Error> {
	let window = app.get_webview_window("main").ok_or_else(|| tauri::Error::WebviewNotFound)?;
	window.hide()?;

	#[cfg(target_os = "macos")]
	{
		use tauri::ActivationPolicy;
		let _ = app.set_activation_policy(ActivationPolicy::Accessory);
	}

	Ok(())
}

#[tokio::main]
async fn main() {
	log_panics::init();
	let _ = fix_path_env::fix();

	#[cfg(target_os = "linux")]
	// SAFETY: std::env::set_var can cause race conditions in multithreaded contexts. We have not spawned any other threads at this point.
	unsafe {
		std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
		std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
	}

	let app = match Builder::default()
		.invoke_handler(tauri::generate_handler![
			frontend::restart,
			frontend::get_devices,
			frontend::get_port_base,
			frontend::get_categories,
			frontend::get_localisations,
			frontend::get_applications,
			frontend::get_application_profiles,
			frontend::set_application_profiles,
			frontend::instances::create_instance,
			frontend::instances::move_instance,
			frontend::instances::remove_instance,
			frontend::instances::set_state,
			frontend::instances::update_image,
			frontend::profiles::get_profiles,
			frontend::profiles::get_selected_profile,
			frontend::profiles::set_selected_profile,
			frontend::profiles::delete_profile,
			frontend::profiles::rename_profile,
			frontend::property_inspector::make_info,
			frontend::property_inspector::switch_property_inspector,
			frontend::property_inspector::open_url,
			frontend::plugins::list_plugins,
			frontend::plugins::install_plugin,
			frontend::plugins::remove_plugin,
			frontend::plugins::reload_plugin,
			frontend::plugins::show_settings_interface,
			frontend::settings::get_settings,
			frontend::settings::set_settings,
			frontend::settings::open_config_directory,
			frontend::settings::open_log_directory,
			frontend::settings::get_build_info
		])
		.setup(|app| {
			APP_HANDLE.set(app.handle().clone()).unwrap();

			#[cfg(windows)]
			if !std::env::args().any(|v| v == "--hide") {
				let _ = app.get_webview_window("main").unwrap().show();
			}
			#[cfg(not(windows))]
			if std::env::args().any(|v| v == "--hide") {
				let _ = hide_window(app.handle());
			}

			// One-time migration from OpenDeck → RiverDeck.
			migrate_opendeck_to_riverdeck(app);

			let mut settings = store::get_settings()?;
			use std::cmp::Ordering;
			use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
			let current_version = semver::Version::parse(built_info::PKG_VERSION)?;
			let settings_version = semver::Version::parse(&settings.value.version)?;
			let cmp = (current_version.major, current_version.minor).cmp(&(settings_version.major, settings_version.minor));
			match cmp {
				Ordering::Less => {
					app.get_webview_window("main").unwrap().close().unwrap();
					app.dialog()
						.message(format!(
							"A newer version of {PRODUCT_NAME} created configuration files on this device. This version is v{}; please upgrade to v{} or newer.",
							built_info::PKG_VERSION,
							settings.value.version
						))
						.title(format!("{PRODUCT_NAME} upgrade required"))
						.kind(MessageDialogKind::Error)
						.show(|_| APP_HANDLE.get().unwrap().exit(1));
					return Ok(());
				}
				Ordering::Greater => {
					let old_version = settings.value.version.clone();
					settings.value.version = built_info::PKG_VERSION.to_owned();
					settings.save()?;
					if old_version == "0.0.0" {
						app.dialog()
							.message(format!(
								r#"Thanks for installing {PRODUCT_NAME}!
If you have any issues, please reach out on any of the support channels listed on GitHub (and make sure to star the project while you're there!).

Some minimal statistics (such as operating system and plugins installed) will be collected from the next time the app starts.
If you do not wish to support development in this way, please disable statistics in the settings.

Enjoy!"#,
							))
							.title(format!("{PRODUCT_NAME} has successfully been installed"))
							.kind(MessageDialogKind::Info)
							.show(|_| ());
						settings.value.statistics = false;
					} else {
						app.dialog()
							.message(format!(
								r#"{PRODUCT_NAME} has been updated to v{}!
Every update brings features, bug fixes, and other improvements, which I spend my time implementing for free.

If you spent $125 on your hardware, please consider spending $10 on the software that makes it work.
You can donate to support development with just a few clicks on GitHub Sponsors.
If you have already donated, thank you so much for your support!"#,
								built_info::PKG_VERSION
							))
							.title(format!("{PRODUCT_NAME} has successfully been updated"))
							.kind(MessageDialogKind::Info)
							.show(|_| ());
					}
				}
				_ => {}
			}

			use tauri_plugin_aptabase::{Builder, EventTracker, InitOptions};
			// Telemetry is opt-in and requires RiverDeck-owned env vars; otherwise this stays disabled.
			let aptabase_app_key = if settings.value.statistics { std::env::var("RIVERDECK_APTABASE_APP_KEY").unwrap_or_default() } else { String::new() };
			let aptabase_host = std::env::var("RIVERDECK_APTABASE_HOST").ok();
			app.handle().plugin(
				Builder::new(&aptabase_app_key)
					.with_options(InitOptions {
						host: aptabase_host,
						flush_interval: None,
					})
					.build(),
			)?;
			let _ = app.track_event("app_started", None);

			tokio::spawn(async {
				loop {
					elgato::initialise_devices().await;
					tokio::time::sleep(std::time::Duration::from_secs(10)).await;
				}
			});
			plugins::initialise_plugins();
			application_watcher::init_application_watcher();

			let label = IconMenuItemBuilder::with_id("label", PRODUCT_NAME)
				.icon(app.default_window_icon().unwrap().clone())
				.enabled(false)
				.build(app)?;
			let show = MenuItemBuilder::with_id("show", "Show").build(app)?;
			let hide = MenuItemBuilder::with_id("hide", "Hide").build(app)?;
			let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;
			let separator = PredefinedMenuItem::separator(app)?;
			let menu = MenuBuilder::new(app).items(&[&label, &separator, &show, &hide, &separator, &quit]).build()?;
			let _tray = TrayIconBuilder::new()
				.menu(&menu)
				.icon(app.default_window_icon().unwrap().clone())
				.show_menu_on_left_click(false)
				.on_tray_icon_event(move |icon, event| {
					if let TrayIconEvent::Click { button, button_state, .. } = event {
						if button != MouseButton::Left || button_state != MouseButtonState::Down {
							return;
						}

						let app_handle = icon.app_handle();
						let window = app_handle.get_webview_window("main").unwrap();
						let _ = if window.is_visible().unwrap_or(false) { hide_window(app_handle) } else { show_window(app_handle) };
					}
				})
				.on_menu_event(move |app, event| {
					let _ = match event.id().as_ref() {
						"show" => show_window(app),
						"hide" => hide_window(app),
						"quit" => {
							app.exit(0);
							Ok(())
						}
						_ => Ok(()),
					};
				})
				.build(app)?;

			#[cfg(any(target_os = "linux", all(debug_assertions, windows)))]
			{
				use tauri_plugin_deep_link::DeepLinkExt;
				let _ = app.deep_link().register_all();
			}

			async fn update() -> Result<(), anyhow::Error> {
				let res = reqwest::Client::new()
					.get("https://api.github.com/repos/sulrwin/RiverDeck/releases/latest")
					.header("Accept", "application/vnd.github+json")
					.header("User-Agent", "RiverDeck")
					.send()
					.await?
					.json::<serde_json::Value>()
					.await?;
				let tag_name = res.get("tag_name").unwrap().as_str().unwrap();
				if semver::Version::parse(built_info::PKG_VERSION)?.cmp(&semver::Version::parse(&tag_name[1..])?) == Ordering::Less {
					let app = APP_HANDLE.get().unwrap();
					app.dialog()
						.message(format!(
							"A new version of {PRODUCT_NAME}, {}, is available.\nUpdate description:\n\n{}",
							tag_name,
							res.get("body").map(|v| v.as_str().unwrap()).unwrap_or("No description").trim()
						))
						.title(format!("{PRODUCT_NAME} update available"))
						.show(|_| ());
				}

				Ok(())
			}

			if settings.value.updatecheck {
				tokio::spawn(async {
					if let Err(error) = update().await {
						log::warn!("Failed to update application: {error}");
					}
				});
			}

			Ok(())
		})
		.plugin(
			tauri_plugin_log::Builder::default()
				.targets([Target::new(TargetKind::LogDir { file_name: None }), Target::new(TargetKind::Stdout)])
				.level(log::LevelFilter::Info)
				.level_for("riverdeck", log::LevelFilter::Trace)
				.build(),
		)
		.plugin(tauri_plugin_cors_fetch::init())
		.plugin(tauri_plugin_single_instance::init(|app, args, _| {
			if let Some(pos) = args.iter().position(|x| x.starts_with("openaction://") || x.starts_with("streamdeck://"))
				&& let Ok(url) = reqwest::Url::parse(&args[pos])
				&& let Some(mut path) = url.path_segments()
			{
				if url.host_str() == Some("plugins")
					&& path.next() == Some("message")
					&& let Some(plugin_id) = path.next()
				{
					if !url.query_pairs().any(|(k, v)| k == url.scheme() && v == "hidden") {
						let _ = show_window(app);
					}

					let plugin_id = if url.scheme() == "streamdeck" { format!("{plugin_id}.sdPlugin") } else { plugin_id.to_owned() };
					tauri::async_runtime::spawn(async move {
						if let Err(error) = events::outbound::deep_link::did_receive_deep_link(&plugin_id, args[pos].clone()).await {
							log::error!("Failed to process deep link for plugin {plugin_id}: {error}");
						}
					});
				}
			} else if let Some(pos) = args.iter().position(|x| x.to_lowercase().trim() == "--reload-plugin") {
				if args.len() > pos + 1 {
					tauri::async_runtime::spawn(frontend::plugins::reload_plugin(app.clone(), args[pos + 1].clone()));
				}
			} else if let Some(pos) = args.iter().position(|x| x.to_lowercase().trim() == "--process-message") {
				if args.len() > pos + 1 {
					tauri::async_runtime::spawn(events::inbound::process_incoming_message(
						Ok(tokio_tungstenite::tungstenite::Message::Text(args[pos + 1].clone().into())),
						"",
						true,
					));
				}
			} else {
				let _ = show_window(app);
			}
		}))
		.plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, Some(vec!["--hide"])))
		.plugin(tauri_plugin_dialog::init())
		.plugin(tauri_plugin_deep_link::init())
		.on_window_event(|window, event| {
			if window.label() != "main" {
				return;
			}
			if let WindowEvent::CloseRequested { api, .. } = event {
				if let Ok(true) = store::get_settings().map(|store| store.value.background) {
					let _ = hide_window(window.app_handle());
					api.prevent_close();
				} else {
					window.app_handle().exit(0);
				}
			}
		})
		.build(tauri::generate_context!())
	{
		Ok(app) => app,
		Err(error) => panic!("failed to build Tauri application: {}", error),
	};

	app.run(|app, event| {
		if let tauri::RunEvent::Exit = event {
			#[cfg(windows)]
			futures::executor::block_on(plugins::deactivate_plugins());
			tokio::spawn(elgato::reset_devices());
			use tauri_plugin_aptabase::EventTracker;
			app.flush_events_blocking();
		}
	});
}
