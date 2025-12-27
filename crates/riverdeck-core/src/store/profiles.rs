use super::Store;

use crate::shared::{ActionInstance, DEVICES, DeviceInfo, Page, Profile, config_dir};

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, anyhow};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::path::Component;
use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

fn validate_profile_id(id: &str) -> Result<(), anyhow::Error> {
    let p = std::path::Path::new(id);
    if p.as_os_str().is_empty() {
        return Err(anyhow!("profile id is empty"));
    }
    for c in p.components() {
        match c {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(anyhow!("invalid profile id"));
            }
            _ => {}
        }
    }
    Ok(())
}

pub struct ProfileStores {
    stores: HashMap<String, Store<Profile>>,
}

fn default_page() -> Page {
    Page {
        id: "1".to_owned(),
        keys: Vec::new(),
        sliders: Vec::new(),
        encoder_screen_background: None,
    }
}

fn ensure_page<'a>(profile: &'a mut Profile, page_id: &str) -> &'a mut Page {
    if let Some(idx) = profile.pages.iter().position(|p| p.id == page_id) {
        return &mut profile.pages[idx];
    }
    profile.pages.push(Page {
        id: page_id.to_owned(),
        keys: Vec::new(),
        sliders: Vec::new(),
        encoder_screen_background: None,
    });
    let idx = profile.pages.len().saturating_sub(1);
    &mut profile.pages[idx]
}

fn get_page<'a>(profile: &'a Profile, page_id: &str) -> Option<&'a Page> {
    profile.pages.iter().find(|p| p.id == page_id)
}

impl ProfileStores {
    fn canonical_id(device: &str, id: &str) -> String {
        if cfg!(target_os = "windows") {
            PathBuf::from(device)
                .join(id.replace('/', "\\"))
                .to_str()
                .unwrap()
                .to_owned()
        } else {
            PathBuf::from(device).join(id).to_str().unwrap().to_owned()
        }
    }

    pub fn get_profile_store(
        &self,
        device: &DeviceInfo,
        id: &str,
    ) -> Result<&Store<Profile>, anyhow::Error> {
        validate_profile_id(id)?;
        self.stores
            .get(&Self::canonical_id(&device.id, id))
            .ok_or_else(|| anyhow!("profile not found"))
    }

    pub async fn get_profile_store_mut(
        &mut self,
        device: &DeviceInfo,
        id: &str,
    ) -> Result<&mut Store<Profile>, anyhow::Error> {
        validate_profile_id(id)?;
        let canonical_id = Self::canonical_id(&device.id, id);
        if self.stores.contains_key(&canonical_id) {
            Ok(self.stores.get_mut(&canonical_id).unwrap())
        } else {
            let default = Profile {
                id: id.to_owned(),
                pages: vec![default_page()],
                selected_page: "1".to_owned(),
            };

            let mut store =
                Store::new(&canonical_id, &config_dir().join("profiles"), default).context(
                    format!("Failed to create store for profile {}", canonical_id),
                )?;
            // Ensure at least one page exists.
            if store.value.pages.is_empty() {
                store.value.pages.push(default_page());
            }
            if store.value.selected_page.trim().is_empty() {
                store.value.selected_page = "1".to_owned();
            }
            // Resize slots for all pages (best effort).
            let key_count = (device.rows * device.columns) as usize;
            for page in store.value.pages.iter_mut() {
                page.keys.resize(key_count, None);
                page.sliders.resize(device.encoders as usize, None);
            }

            let categories = crate::shared::CATEGORIES.read().await;
            let actions = categories
                .values()
                .flat_map(|v| v.actions.iter())
                .collect::<Vec<_>>();
            let plugins_dir = config_dir().join("plugins");
            let registered = crate::events::registered_plugins().await;
            let keep_instance = |instance: &ActionInstance| -> bool {
                instance.action.plugin == "riverdeck"
                    || (plugins_dir.join(&instance.action.plugin).exists()
                        && (!registered.contains(&instance.action.plugin)
                            || actions.iter().any(|v| v.uuid == instance.action.uuid)))
            };
            for page in store.value.pages.iter_mut() {
                for slot in page.keys.iter_mut().chain(page.sliders.iter_mut()) {
                    if let Some(instance) = slot {
                        if !keep_instance(instance) {
                            *slot = None;
                        } else if let Some(children) = &mut instance.children {
                            children.retain_mut(|child| keep_instance(child));
                        }
                    }
                }
            }
            store.save()?;

            self.stores.insert(canonical_id.clone(), store);
            Ok(self.stores.get_mut(&canonical_id).unwrap())
        }
    }

    pub fn remove_profile(&mut self, device: &str, id: &str) {
        if validate_profile_id(id).is_err() {
            return;
        }
        self.stores.remove(&Self::canonical_id(device, id));
    }

    pub fn delete_profile(&mut self, device: &str, id: &str) {
        if validate_profile_id(id).is_err() {
            return;
        }
        self.remove_profile(device, id);
        let config_dir = config_dir();
        #[cfg(target_os = "windows")]
        let id = &id.replace('/', "\\");
        let path = config_dir
            .join("profiles")
            .join(device)
            .join(format!("{id}.json"));
        let _ = fs::remove_file(&path);
        // This is safe as `remove_dir` errors if the directory is not empty.
        let _ = fs::remove_dir(path.parent().unwrap());
        let images_path = config_dir.join("images").join(device).join(id);
        let _ = fs::remove_dir_all(images_path);
    }

    pub async fn rename_profile(
        &mut self,
        device: &DeviceInfo,
        old_id: &str,
        new_id: &str,
    ) -> Result<(), anyhow::Error> {
        validate_profile_id(old_id)?;
        validate_profile_id(new_id)?;
        // Remove from the store but don't delete the file
        self.remove_profile(&device.id, old_id);

        let config_dir = config_dir();

        // Construct old and new paths (handling Windows path separators)
        #[cfg(target_os = "windows")]
        let old_path_id = old_id.replace('/', "\\");
        #[cfg(not(target_os = "windows"))]
        let old_path_id = old_id;

        #[cfg(target_os = "windows")]
        let new_path_id = new_id.replace('/', "\\");
        #[cfg(not(target_os = "windows"))]
        let new_path_id = new_id;

        let old_path = config_dir
            .join("profiles")
            .join(&device.id)
            .join(format!("{}.json", old_path_id));
        let new_path = config_dir
            .join("profiles")
            .join(&device.id)
            .join(format!("{}.json", new_path_id));

        // Create parent directory for new path if it doesn't exist
        if let Some(parent) = new_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Rename the profile file
        fs::rename(&old_path, &new_path)?;

        // Clean up empty old directory if profile was in a folder
        if let Some(parent) = old_path.parent() {
            // This is safe as `remove_dir` errors if the directory is not empty.
            let _ = fs::remove_dir(parent);
        }

        // Rename images directory if it exists
        let old_images_path = config_dir.join("images").join(&device.id).join(old_path_id);
        let new_images_path = config_dir.join("images").join(&device.id).join(new_path_id);

        if old_images_path.exists() {
            if let Some(parent) = new_images_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(&old_images_path, &new_images_path)?;

            // Clean up empty old images directory
            if let Some(parent) = old_images_path.parent() {
                // This is safe as `remove_dir` errors if the directory is not empty.
                let _ = fs::remove_dir(parent);
            }
        }

        // Reload the new profile
        self.get_profile_store_mut(device, new_id).await?;

        Ok(())
    }

    pub fn all_from_plugin(&self, plugin: &str) -> Vec<crate::shared::ActionContext> {
        let mut all = vec![];
        for store in self.stores.values() {
            for page in store.value.pages.iter() {
                for instance in page
                    .keys
                    .iter()
                    .flatten()
                    .chain(page.sliders.iter().flatten())
                {
                    if instance.action.plugin == plugin {
                        all.push(instance.context.clone());
                    }
                }
            }
        }
        all
    }
}

#[derive(Serialize, Deserialize)]
pub struct DeviceConfig {
    pub selected_profile: String,
}

impl super::NotProfile for DeviceConfig {}

pub struct DeviceStores {
    stores: HashMap<String, Store<DeviceConfig>>,
}

impl DeviceStores {
    pub fn get_selected_profile(&mut self, device: &str) -> Result<String, anyhow::Error> {
        if !self.stores.contains_key(device) {
            let default = DeviceConfig {
                selected_profile: "Default".to_owned(),
            };

            let store = Store::new(device, &config_dir().join("profiles"), default).context(
                format!("Failed to create store for device config {}", device),
            )?;
            store.save()?;

            self.stores.insert(device.to_owned(), store);
        }

        let from_store = &self.stores.get(device).unwrap().value.selected_profile;
        let all = get_device_profiles(device)?;
        if all.contains(from_store) {
            Ok(from_store.clone())
        } else {
            Ok(all.first().unwrap().clone())
        }
    }

    pub fn set_selected_profile(&mut self, device: &str, id: String) -> Result<(), anyhow::Error> {
        if self.stores.contains_key(device) {
            let store = self.stores.get_mut(device).unwrap();
            store.value.selected_profile = id;
            store.save()?;
        } else {
            let default = DeviceConfig {
                selected_profile: id,
            };

            let store = Store::new(device, &config_dir().join("profiles"), default).context(
                format!("Failed to create store for device config {}", device),
            )?;
            store.save()?;

            self.stores.insert(device.to_owned(), store);
        }
        Ok(())
    }
}

fn canonical_profile_id(device: &str, id: &str) -> String {
    if cfg!(target_os = "windows") {
        PathBuf::from(device)
            .join(id.replace('/', "\\"))
            .to_str()
            .unwrap()
            .to_owned()
    } else {
        PathBuf::from(device).join(id).to_str().unwrap().to_owned()
    }
}

fn selected_page_mut(profile: &mut Profile) -> &mut Page {
    if profile.pages.is_empty() {
        profile.pages.push(default_page());
    }
    if profile.selected_page.trim().is_empty() {
        profile.selected_page = "1".to_owned();
    }
    let sel = profile.selected_page.clone();
    if let Some(idx) = profile.pages.iter().position(|p| p.id == sel) {
        &mut profile.pages[idx]
    } else {
        &mut profile.pages[0]
    }
}

fn rewrite_instance_context(
    inst: &mut ActionInstance,
    new_profile: &str,
    new_page: &str,
    old_images_dir: Option<&std::path::Path>,
    new_images_dir: Option<&std::path::Path>,
) {
    inst.context.profile = new_profile.to_owned();
    inst.context.page = new_page.to_owned();

    if let (Some(old_base), Some(new_base)) = (old_images_dir, new_images_dir) {
        for st in inst.states.iter_mut() {
            let p = std::path::Path::new(st.image.trim());
            if p.starts_with(old_base)
                && let Ok(rel) = p.strip_prefix(old_base)
            {
                st.image = new_base.join(rel).to_string_lossy().into_owned();
            }
        }
    }

    if let Some(children) = inst.children.as_mut() {
        for c in children.iter_mut() {
            rewrite_instance_context(c, new_profile, new_page, old_images_dir, new_images_dir);
        }
    }
}

fn move_dir_contents_best_effort(
    src: &std::path::Path,
    dst: &std::path::Path,
) -> anyhow::Result<()> {
    if !src.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        // If `dst` is inside `src` (e.g. migrating to `.../<profile>/1`), avoid trying to move
        // the destination directory into itself.
        if from == dst {
            continue;
        }
        let to = dst.join(entry.file_name());
        if std::fs::rename(&from, &to).is_err() {
            if from.is_dir() {
                let _ = crate::shared::copy_dir(&from, &to);
                let _ = std::fs::remove_dir_all(&from);
            } else {
                let _ = std::fs::copy(&from, &to);
                let _ = std::fs::remove_file(&from);
            }
        }
    }
    Ok(())
}

fn migrate_device_profiles_to_pages(device: &str) -> Result<(), anyhow::Error> {
    let cfg = config_dir();
    let device_profiles_dir = cfg.join("profiles").join(device);
    std::fs::create_dir_all(&device_profiles_dir)?;
    let marker = device_profiles_dir.join(".pages_migrated_v1");
    if marker.exists() {
        return Ok(());
    }

    let existing = get_device_profiles_raw(device)?;

    // Helper: load legacy/v2 profile using Store parsing.
    let load_profile = |id: &str| -> Result<Profile, anyhow::Error> {
        let canonical = canonical_profile_id(device, id);
        let default = Profile {
            id: id.to_owned(),
            pages: vec![default_page()],
            selected_page: "1".to_owned(),
        };
        let store = Store::new(&canonical, &cfg.join("profiles"), default)?;
        Ok(store.value)
    };

    let mut page_sources: Vec<(String, String)> = vec![]; // (old_profile_id, new_page_id)
    if existing.iter().any(|p| p == "Default") {
        page_sources.push(("Default".to_owned(), "1".to_owned()));
    }
    for id in existing.iter() {
        if let Some(n) = id.strip_prefix("Page ").and_then(|n| n.parse::<u32>().ok()) {
            page_sources.push((id.clone(), n.to_string()));
        }
    }
    page_sources.sort_by(|a, b| {
        let pa = a.1.parse::<u32>().unwrap_or(0);
        let pb = b.1.parse::<u32>().unwrap_or(0);
        pa.cmp(&pb)
    });

    let has_grouped_pages = page_sources.len() > 1;
    if has_grouped_pages {
        // Build grouped Default profile.
        let mut grouped = Profile {
            id: "Default".to_owned(),
            pages: vec![],
            selected_page: "1".to_owned(),
        };

        for (old_id, new_page_id) in page_sources.iter() {
            let mut prof = load_profile(old_id)?;
            let mut page = selected_page_mut(&mut prof).clone();
            page.id = new_page_id.clone();

            let old_images_dir = cfg.join("images").join(device).join(old_id);
            let new_images_dir = cfg
                .join("images")
                .join(device)
                .join("Default")
                .join(new_page_id);
            if old_id == "Default" {
                // Move legacy Default image contents into Default/1.
                let legacy_root = cfg.join("images").join(device).join("Default");
                let _ = move_dir_contents_best_effort(&legacy_root, &new_images_dir);
            } else if old_images_dir.exists() {
                std::fs::create_dir_all(new_images_dir.parent().unwrap())?;
                if std::fs::rename(&old_images_dir, &new_images_dir).is_err() {
                    let _ = crate::shared::copy_dir(&old_images_dir, &new_images_dir);
                    let _ = std::fs::remove_dir_all(&old_images_dir);
                }
            }

            for slot in page.keys.iter_mut().chain(page.sliders.iter_mut()) {
                if let Some(inst) = slot.as_mut() {
                    rewrite_instance_context(
                        inst,
                        "Default",
                        new_page_id,
                        Some(&old_images_dir),
                        Some(&new_images_dir),
                    );
                }
            }
            if let Some(bg) = page.encoder_screen_background.as_mut() {
                let p = std::path::Path::new(bg.trim());
                if p.starts_with(&old_images_dir)
                    && let Ok(rel) = p.strip_prefix(&old_images_dir)
                {
                    *bg = new_images_dir.join(rel).to_string_lossy().into_owned();
                }
            }

            grouped.pages.push(page);
        }

        // If the selected profile was "Page N", map it into Default.selected_page.
        let mut device_cfg = Store::new(
            device,
            &cfg.join("profiles"),
            DeviceConfig {
                selected_profile: "Default".to_owned(),
            },
        )?;
        if let Some(n) = device_cfg.value.selected_profile.strip_prefix("Page ")
            && let Ok(n) = n.parse::<u32>()
        {
            device_cfg.value.selected_profile = "Default".to_owned();
            grouped.selected_page = n.to_string();
        }

        // Persist grouped Default.
        let canonical = canonical_profile_id(device, "Default");
        let mut store = Store::new(
            &canonical,
            &cfg.join("profiles"),
            Profile {
                id: "Default".to_owned(),
                pages: vec![default_page()],
                selected_page: "1".to_owned(),
            },
        )?;
        store.value = grouped;
        store.save()?;
        device_cfg.save()?;

        // Remove legacy Page N profile JSON files (keep images already moved).
        for (old_id, _) in page_sources.iter() {
            if old_id != "Default" && old_id.starts_with("Page ") {
                #[cfg(target_os = "windows")]
                let old_path_id = old_id.replace('/', "\\");
                #[cfg(not(target_os = "windows"))]
                let old_path_id = old_id.as_str();
                let p = cfg
                    .join("profiles")
                    .join(device)
                    .join(format!("{old_path_id}.json"));
                let _ = std::fs::remove_file(p);
            }
        }
    }

    // Normalize remaining profiles to v2 + ensure images live under page "1".
    let remaining = get_device_profiles_raw(device)?;
    for id in remaining.iter() {
        if id.starts_with("Page ") {
            continue;
        }
        let mut prof = load_profile(id)?;
        if prof.pages.is_empty() {
            prof.pages.push(default_page());
        }
        if prof.pages.len() == 1 {
            prof.pages[0].id = "1".to_owned();
            prof.selected_page = "1".to_owned();
        }

        let legacy_root = cfg.join("images").join(device).join(id);
        let page_root = cfg.join("images").join(device).join(id).join("1");
        if legacy_root.is_dir() && !page_root.exists() {
            let _ = move_dir_contents_best_effort(&legacy_root, &page_root);
        }
        let old_images_dir = legacy_root;
        let new_images_dir = page_root;

        for page in prof.pages.iter_mut() {
            let pid = page.id.clone();
            for slot in page.keys.iter_mut().chain(page.sliders.iter_mut()) {
                if let Some(inst) = slot.as_mut() {
                    rewrite_instance_context(
                        inst,
                        id,
                        &pid,
                        Some(&old_images_dir),
                        Some(&new_images_dir),
                    );
                }
            }
            if let Some(bg) = page.encoder_screen_background.as_mut() {
                let p = std::path::Path::new(bg.trim());
                if p.starts_with(&old_images_dir)
                    && let Ok(rel) = p.strip_prefix(&old_images_dir)
                {
                    *bg = new_images_dir.join(rel).to_string_lossy().into_owned();
                }
            }
        }

        let canonical = canonical_profile_id(device, id);
        let mut store = Store::new(
            &canonical,
            &cfg.join("profiles"),
            Profile {
                id: id.to_owned(),
                pages: vec![default_page()],
                selected_page: "1".to_owned(),
            },
        )?;
        store.value = prof;
        store.save()?;
    }

    let _ = std::fs::write(&marker, b"");
    Ok(())
}

fn get_device_profiles_raw(device: &str) -> Result<Vec<String>, anyhow::Error> {
    let mut profiles: Vec<String> = vec![];

    let device_path = config_dir().join("profiles").join(device);
    fs::create_dir_all(&device_path)?;
    let entries = fs::read_dir(device_path)?;

    for entry in entries.flatten() {
        if entry.metadata()?.is_file() {
            let mut id = entry.file_name().to_string_lossy().into_owned();
            if id.ends_with(".json") {
                id.truncate(id.len() - 5);
            } else if id.ends_with(".json.bak") {
                id.truncate(id.len() - 9);
            } else if id.ends_with(".json.temp") {
                id.truncate(id.len() - 10);
            } else {
                continue;
            }
            profiles.push(id);
        } else if entry.metadata()?.is_dir() {
            let entries = fs::read_dir(entry.path())?;
            for subentry in entries.flatten() {
                if subentry.metadata()?.is_file() {
                    let mut id = format!(
                        "{}/{}",
                        entry.file_name().to_string_lossy(),
                        &subentry.file_name().to_string_lossy()
                    );
                    if id.ends_with(".json") {
                        id.truncate(id.len() - 5);
                    } else if id.ends_with(".json.bak") {
                        id.truncate(id.len() - 9);
                    } else if id.ends_with(".json.temp") {
                        id.truncate(id.len() - 10);
                    } else {
                        continue;
                    }
                    profiles.push(id);
                }
            }
        }
    }

    if profiles.is_empty() {
        profiles.push("Default".to_owned());
    }

    Ok(profiles)
}

pub fn get_device_profiles(device: &str) -> Result<Vec<String>, anyhow::Error> {
    // Best-effort migration; if it fails we still return whatever exists on disk.
    let _ = migrate_device_profiles_to_pages(device);
    let mut profiles = get_device_profiles_raw(device)?;
    // After migration, legacy "Page N" profiles should not appear as top-level profiles.
    profiles.retain(|p| !p.starts_with("Page "));
    Ok(profiles)
}

/// A singleton object to contain all active Store instances that hold a profile.
pub static PROFILE_STORES: Lazy<RwLock<ProfileStores>> = Lazy::new(|| {
    RwLock::new(ProfileStores {
        stores: HashMap::new(),
    })
});

/// A singleton object to manage Store instances for device configurations.
pub static DEVICE_STORES: Lazy<RwLock<DeviceStores>> = Lazy::new(|| {
    RwLock::new(DeviceStores {
        stores: HashMap::new(),
    })
});

pub struct Locks<'a> {
    #[allow(dead_code)]
    pub device_stores: RwLockReadGuard<'a, DeviceStores>,
    pub profile_stores: RwLockReadGuard<'a, ProfileStores>,
}

pub async fn acquire_locks() -> Locks<'static> {
    let device_stores = DEVICE_STORES.read().await;
    let profile_stores = PROFILE_STORES.read().await;
    Locks {
        device_stores,
        profile_stores,
    }
}

pub struct LocksMut<'a> {
    pub device_stores: RwLockWriteGuard<'a, DeviceStores>,
    pub profile_stores: RwLockWriteGuard<'a, ProfileStores>,
}

pub async fn acquire_locks_mut() -> LocksMut<'static> {
    let device_stores = DEVICE_STORES.write().await;
    let profile_stores = PROFILE_STORES.write().await;
    LocksMut {
        device_stores,
        profile_stores,
    }
}

pub async fn get_slot<'a>(
    context: &crate::shared::Context,
    locks: &'a Locks<'_>,
) -> Result<&'a Option<crate::shared::ActionInstance>, anyhow::Error> {
    let device = DEVICES
        .get(&context.device)
        .ok_or_else(|| anyhow!("device not found"))?;
    let store = locks
        .profile_stores
        .get_profile_store(&device, &context.profile)?;

    let page = get_page(&store.value, &context.page).ok_or_else(|| anyhow!("page not found"))?;
    let configured = match &context.controller[..] {
        "Encoder" => page
            .sliders
            .get(context.position as usize)
            .ok_or_else(|| anyhow!("index out of bounds"))?,
        _ => page
            .keys
            .get(context.position as usize)
            .ok_or_else(|| anyhow!("index out of bounds"))?,
    };

    Ok(configured)
}

pub async fn get_slot_mut<'a>(
    context: &crate::shared::Context,
    locks: &'a mut LocksMut<'_>,
) -> Result<&'a mut Option<crate::shared::ActionInstance>, anyhow::Error> {
    let device = DEVICES
        .get(&context.device)
        .ok_or_else(|| anyhow!("device not found"))?;
    let store = locks
        .profile_stores
        .get_profile_store_mut(&device, &context.profile)
        .await?;

    // Ensure the requested page exists and slots are sized.
    let key_count = (device.rows * device.columns) as usize;
    let enc_count = device.encoders as usize;
    let page = ensure_page(&mut store.value, &context.page);
    page.keys.resize(key_count, None);
    page.sliders.resize(enc_count, None);

    let configured = match &context.controller[..] {
        "Encoder" => page
            .sliders
            .get_mut(context.position as usize)
            .ok_or_else(|| anyhow!("index out of bounds"))?,
        _ => page
            .keys
            .get_mut(context.position as usize)
            .ok_or_else(|| anyhow!("index out of bounds"))?,
    };

    Ok(configured)
}

pub async fn get_instance<'a>(
    context: &crate::shared::ActionContext,
    locks: &'a Locks<'_>,
) -> Result<Option<&'a crate::shared::ActionInstance>, anyhow::Error> {
    let slot = get_slot(&(context.into()), locks).await?;
    if let Some(instance) = slot {
        if instance.context == *context {
            return Ok(Some(instance));
        } else if let Some(children) = &instance.children {
            for child in children {
                if child.context == *context {
                    return Ok(Some(child));
                }
            }
        }
    }
    Ok(None)
}

pub async fn get_instance_mut<'a>(
    context: &crate::shared::ActionContext,
    locks: &'a mut LocksMut<'_>,
) -> Result<Option<&'a mut crate::shared::ActionInstance>, anyhow::Error> {
    let slot = get_slot_mut(&(context.into()), locks).await?;
    if let Some(instance) = slot {
        if instance.context == *context {
            return Ok(Some(instance));
        } else if let Some(children) = &mut instance.children {
            for child in children {
                if child.context == *context {
                    return Ok(Some(child));
                }
            }
        }
    }
    Ok(None)
}

pub async fn save_profile(device: &str, locks: &mut LocksMut<'_>) -> Result<(), anyhow::Error> {
    let selected_profile = locks.device_stores.get_selected_profile(device)?;
    let device = DEVICES.get(device).unwrap();
    let store = locks
        .profile_stores
        .get_profile_store(&device, &selected_profile)?;
    store.save()
}

#[cfg(test)]
mod tests {
    use super::validate_profile_id;

    #[test]
    fn profile_id_validation_allows_folders_but_not_traversal() {
        assert!(validate_profile_id("Default").is_ok());
        assert!(validate_profile_id("Folder/Profile").is_ok());

        assert!(validate_profile_id("").is_err());
        assert!(validate_profile_id("../evil").is_err());
        assert!(validate_profile_id("Folder/../evil").is_err());
        assert!(validate_profile_id("/abs").is_err());
    }
}
