use crate::shared::{config_dir, log_dir};
use crate::store::profiles::{acquire_locks, get_instance};
use crate::ui::{PluginInstallPhase, UiEvent};

use tokio::fs;

#[derive(serde::Serialize)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub author: String,
    pub icon: String,
    pub version: String,
    pub has_settings_interface: bool,
    pub builtin: bool,
    pub registered: bool,
}

pub async fn list_plugins() -> Result<Vec<PluginInfo>, anyhow::Error> {
    let mut plugins = vec![];

    let plugins_dir = config_dir().join("plugins");
    let plugins_canon = tokio::fs::canonicalize(&plugins_dir)
        .await
        .unwrap_or(plugins_dir.clone());
    let mut entries = fs::read_dir(&plugins_dir).await?;
    let registered = crate::events::registered_plugins().await;

    let builtins: Vec<String> = crate::shared::resource_dir()
        .map(|d| d.join("plugins"))
        .and_then(|d| std::fs::read_dir(d).ok())
        .map(|entries| {
            entries
                .flatten()
                .map(|x| x.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();

    while let Some(entry) = entries.next_entry().await? {
        let entry_path = entry.path();
        let meta = tokio::fs::symlink_metadata(&entry_path).await?;
        let path = if meta.file_type().is_symlink() {
            let target = fs::read_link(&entry_path).await?;
            let target_canon = tokio::fs::canonicalize(&target)
                .await
                .unwrap_or(target.clone());
            if !target_canon.starts_with(&plugins_canon) {
                continue;
            }
            target
        } else {
            entry_path
        };
        let metadata = fs::metadata(&path).await?;
        if metadata.is_dir() {
            let id = path.file_name().unwrap().to_str().unwrap().to_owned();
            let Ok(manifest) = crate::plugins::manifest::read_manifest(&path) else {
                continue;
            };
            plugins.push(PluginInfo {
                name: manifest.name,
                author: manifest.author,
                icon: crate::shared::convert_icon(
                    path.join(manifest.icon).to_str().unwrap().to_owned(),
                ),
                version: manifest.version,
                has_settings_interface: manifest.has_settings_interface.unwrap_or(false),
                builtin: builtins.contains(&id),
                registered: registered.contains(&id),
                id,
            });
        }
    }

    Ok(plugins)
}

pub async fn install_plugin(
    url: Option<String>,
    file: Option<String>,
    fallback_id: Option<String>,
) -> Result<(), anyhow::Error> {
    let bytes = match file {
        None => {
            let url = url.ok_or_else(|| anyhow::anyhow!("missing url"))?;
            // Best-effort scheme restriction (avoid surprising `file://` style behavior).
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                return Err(anyhow::anyhow!("unsupported url scheme"));
            }
            let parsed = reqwest::Url::parse(&url).ok();
            let host = parsed
                .as_ref()
                .and_then(|u| u.host_str())
                .unwrap_or_default();

            // Some marketplace/CDN downloads require browser-like headers; otherwise they can return
            // HTML/error pages that look like a successful download but aren't a zip archive.
            let client = reqwest::Client::new();
            let mut rb = client.get(&url).header("accept", "*/*");
            if host.ends_with("mp-cdn.elgato.com") || host.ends_with("mp-gateway.elgato.com") {
                rb = rb
                    .header(
                        "user-agent",
                        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) RiverDeck/1.0",
                    )
                    .header("origin", "https://marketplace.elgato.com")
                    .header("referer", "https://marketplace.elgato.com/");
            }

            let resp = rb.send().await?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.bytes().await.unwrap_or_default();
                let preview = String::from_utf8_lossy(&body[..body.len().min(256)]).into_owned();
                return Err(anyhow::anyhow!(
                    "download failed (status={status}) body_preview={preview:?}"
                ));
            }
            use std::ops::Deref;
            let bytes = resp.bytes().await?.deref().to_owned();

            // Quick sanity check: Stream Deck plugin archives are zip files (start with "PK").
            // If this doesn't look like a zip, surface a clearer error before unzip.
            if bytes.len() >= 2 && &bytes[0..2] != b"PK" {
                let preview = String::from_utf8_lossy(&bytes[..bytes.len().min(256)]).into_owned();
                return Err(anyhow::anyhow!(
                    "downloaded file does not look like a zip (missing PK header); body_preview={preview:?}"
                ));
            }
            bytes
        }
        Some(path) => std::fs::read(path)?,
    };

    let id = match crate::zip_extract::dir_name(std::io::Cursor::new(&bytes)) {
        Ok(id) => id,
        Err(error) => match fallback_id {
            Some(id) => format!("{id}.sdPlugin"),
            None => return Err(error.into()),
        },
    };

    crate::ui::emit(UiEvent::PluginInstall {
        id: id.clone(),
        phase: PluginInstallPhase::Started,
    });

    let result: Result<(), anyhow::Error> = async {
        let _ = crate::plugins::deactivate_plugin(&id).await;

        let config_dir = config_dir();
        let plugins_dir = config_dir.join("plugins");
        let actual = plugins_dir.join(&id);

        // Extract into an isolated temp directory, then atomically move the plugin folder into place.
        // This reduces symlink/TOCTOU footguns and ensures we don't partially clobber an existing plugin.
        let temp_root = config_dir.join("temp");
        fs::create_dir_all(&temp_root).await?;
        let extract_root = temp_root.join(format!(
            "extract_{}_{}",
            id.replace('/', "_"),
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&extract_root).await;
        fs::create_dir_all(&extract_root).await?;

        if let Err(error) = crate::zip_extract::extract(std::io::Cursor::new(bytes), &extract_root)
        {
            log::error!("Failed to unzip file: {}", error);
            let _ = fs::remove_dir_all(&extract_root).await;
            return Err(error.into());
        }

        // Find the plugin directory within the extracted tree.
        fn find_plugin_dir(
            root: &std::path::Path,
            name: &str,
            depth: usize,
        ) -> Option<std::path::PathBuf> {
            if depth == 0 {
                return None;
            }
            let entries = std::fs::read_dir(root).ok()?;
            for entry in entries.flatten() {
                let path = entry.path();
                let meta = std::fs::symlink_metadata(&path).ok()?;
                if meta.file_type().is_symlink() {
                    continue;
                }
                if meta.is_dir() {
                    if path.file_name().and_then(|n| n.to_str()) == Some(name) {
                        return Some(path);
                    }
                    if let Some(found) = find_plugin_dir(&path, name, depth - 1) {
                        return Some(found);
                    }
                }
            }
            None
        }

        let extracted_plugin = find_plugin_dir(&extract_root, &id, 6)
            .ok_or_else(|| anyhow::anyhow!("extracted archive did not contain {id}"))?;

        // Backup existing plugin dir, if present.
        let backup = temp_root.join(format!(
            "backup_{}_{}",
            id.replace('/', "_"),
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&backup).await;
        if actual.exists() {
            fs::rename(&actual, &backup).await?;
        }

        // Ensure target parent exists.
        fs::create_dir_all(&plugins_dir).await?;
        if let Err(e) = fs::rename(&extracted_plugin, &actual).await {
            // Restore backup on failure.
            let _ = fs::remove_dir_all(&actual).await;
            if backup.exists() {
                let _ = fs::rename(&backup, &actual).await;
            }
            let _ = fs::remove_dir_all(&extract_root).await;
            return Err(e.into());
        }

        // Initialize and rollback if initialization fails.
        if let Err(error) = crate::plugins::initialise_plugin(&actual).await {
            log::warn!(
                "Failed to initialise plugin at {}: {}",
                actual.display(),
                error
            );
            // If the failure looks like a manifest decoding issue, keep a copy for debugging.
            // This helps us support edge-case encodings found in Marketplace plugins.
            let err_s = error.to_string();
            let looks_like_manifest = err_s.contains("manifest")
                && (err_s.contains("encoding")
                    || err_s.contains("ELGATO")
                    || err_s.contains("binary format"));
            let looks_like_platform = err_s.contains("unsupported on platform");
            if looks_like_manifest || looks_like_platform {
                let bad = temp_root.join(format!(
                    "bad_{}_{}",
                    id.replace('/', "_"),
                    std::process::id()
                ));
                let _ = fs::remove_dir_all(&bad).await;
                if fs::rename(&actual, &bad).await.is_ok() {
                    log::warn!(
                        "Preserved failed plugin install for inspection at {}",
                        bad.display()
                    );
                } else {
                    let _ = fs::remove_dir_all(&actual).await;
                }
            } else {
                let _ = fs::remove_dir_all(&actual).await;
            }
            if backup.exists() {
                let _ = fs::rename(&backup, &actual).await;
                let _ = crate::plugins::initialise_plugin(&actual).await;
            }
            let _ = fs::remove_dir_all(&extract_root).await;
            return Err(error);
        }

        let _ = fs::remove_dir_all(&backup).await;
        let _ = fs::remove_dir_all(&extract_root).await;
        Ok(())
    }
    .await;

    match &result {
        Ok(()) => crate::ui::emit(UiEvent::PluginInstall {
            id,
            phase: PluginInstallPhase::Finished {
                ok: true,
                error: None,
            },
        }),
        Err(err) => crate::ui::emit(UiEvent::PluginInstall {
            id,
            phase: PluginInstallPhase::Finished {
                ok: false,
                error: Some(format!("{err:#}")),
            },
        }),
    }

    result
}

pub async fn remove_plugin(id: String) -> Result<(), anyhow::Error> {
    let locks = acquire_locks().await;
    let all = locks.profile_stores.all_from_plugin(&id);
    drop(locks);

    for context in all {
        super::instances::remove_instance(context).await?;
    }

    crate::plugins::deactivate_plugin(&id).await?;
    fs::remove_dir_all(config_dir().join("plugins").join(&id)).await?;

    let mut categories = crate::shared::CATEGORIES.write().await;
    for category in categories.values_mut() {
        category.actions.retain(|v| v.plugin != id);
    }
    categories.retain(|_, v| !v.actions.is_empty());

    let _ = fs::remove_file(log_dir().join("plugins").join(format!("{id}.log"))).await;
    let _ = fs::remove_file(config_dir().join("settings").join(format!("{id}.json"))).await;

    Ok(())
}

pub async fn reload_plugin(id: String) {
    let _ = crate::plugins::deactivate_plugin(&id).await;
    let _ = crate::plugins::initialise_plugin(&config_dir().join("plugins").join(&id)).await;

    let locks = acquire_locks().await;
    let all = locks.profile_stores.all_from_plugin(&id);

    for context in all {
        if let Ok(Some(instance)) = get_instance(&context, &locks).await {
            let _ = crate::events::outbound::will_appear::will_appear(instance).await;
        }
    }
}

pub async fn show_settings_interface(plugin: String) -> Result<(), anyhow::Error> {
    crate::events::outbound::settings::show_settings_interface(&plugin).await?;
    Ok(())
}
