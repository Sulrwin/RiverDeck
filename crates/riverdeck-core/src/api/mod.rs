pub mod instances;
pub mod plugins;
pub mod profiles;
pub mod property_inspector;
pub mod settings;

use crate::shared::{CATEGORIES, Category, DEVICES, DeviceInfo};

use std::collections::HashMap;

pub fn get_devices() -> dashmap::DashMap<String, DeviceInfo> {
    DEVICES.clone()
}

pub fn get_port_base() -> u16 {
    *crate::plugins::PORT_BASE
}

pub async fn get_categories() -> HashMap<String, Category> {
    CATEGORIES.read().await.clone()
}

pub async fn get_localisations(
    locale: &str,
) -> Result<HashMap<String, serde_json::Value>, anyhow::Error> {
    let mut localisations: HashMap<String, serde_json::Value> = HashMap::new();

    let mut entries = tokio::fs::read_dir(&crate::shared::config_dir().join("plugins")).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = match entry.metadata().await?.is_symlink() {
            true => tokio::fs::read_link(entry.path()).await?,
            false => entry.path(),
        };
        let metadata = tokio::fs::metadata(&path).await?;
        if metadata.is_dir() {
            let Ok(locale_bytes) = tokio::fs::read(path.join(format!("{locale}.json"))).await
            else {
                continue;
            };
            let Ok(locale): Result<serde_json::Value, _> = serde_json::from_slice(&locale_bytes)
            else {
                continue;
            };
            localisations.insert(
                path.file_name().unwrap().to_str().unwrap().to_owned(),
                locale,
            );
        }
    }

    Ok(localisations)
}

pub async fn get_applications() -> Vec<String> {
    crate::application_watcher::APPLICATIONS
        .read()
        .await
        .clone()
}

pub async fn get_application_profiles() -> crate::application_watcher::ApplicationProfiles {
    crate::application_watcher::APPLICATION_PROFILES
        .read()
        .await
        .value
        .clone()
}

pub async fn set_application_profiles(
    value: crate::application_watcher::ApplicationProfiles,
) -> Result<(), anyhow::Error> {
    let mut store = crate::application_watcher::APPLICATION_PROFILES
        .write()
        .await;
    store.value = value;
    store.save()?;
    Ok(())
}
