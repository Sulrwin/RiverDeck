use crate::shared::{config_dir, log_dir};
use crate::store::profiles::{acquire_locks, get_instance};

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

    let mut entries = fs::read_dir(&config_dir().join("plugins")).await?;
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
        let path = match entry.metadata().await?.is_symlink() {
            true => fs::read_link(entry.path()).await?,
            false => entry.path(),
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
            let resp = reqwest::get(url.ok_or_else(|| anyhow::anyhow!("missing url"))?).await?;
            use std::ops::Deref;
            resp.bytes().await?.deref().to_owned()
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

    let _ = crate::plugins::deactivate_plugin(&id).await;

    let config_dir = config_dir();
    let actual = config_dir.join("plugins").join(&id);

    if actual.exists() {
        let _ = fs::create_dir_all(config_dir.join("temp")).await;
    }
    let temp = config_dir.join("temp").join(&id);
    let _ = fs::rename(&actual, &temp).await;

    if let Err(error) =
        crate::zip_extract::extract(std::io::Cursor::new(bytes), &config_dir.join("plugins"))
    {
        log::error!("Failed to unzip file: {}", error);
        let _ = fs::rename(&temp, &actual).await;
        let _ = crate::plugins::initialise_plugin(&actual).await;
        return Err(error.into());
    }
    if let Err(error) = crate::plugins::initialise_plugin(&actual).await {
        log::warn!(
            "Failed to initialise plugin at {}: {}",
            actual.display(),
            error
        );
        let _ = fs::remove_dir_all(&actual).await;
        let _ = fs::rename(&temp, &actual).await;
        let _ = crate::plugins::initialise_plugin(&actual).await;
        return Err(error);
    }
    let _ = fs::remove_dir_all(config_dir.join("temp")).await;

    Ok(())
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
