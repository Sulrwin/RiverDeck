use crate::shared::DEVICES;
use crate::store::profiles::{PROFILE_STORES, acquire_locks_mut, get_device_profiles};

pub fn get_profiles(device: &str) -> Result<Vec<String>, anyhow::Error> {
    get_device_profiles(device)
}

pub async fn create_profile(
    device: String,
    id: String,
    clone_from: Option<String>,
) -> Result<(), anyhow::Error> {
    use std::path::Path;

    let mut locks = acquire_locks_mut().await;
    if !DEVICES.contains_key(&device) {
        return Err(anyhow::anyhow!("device {device} not found"));
    }

    let dev = DEVICES.get(&device).unwrap();
    // If cloning, grab the source profile value first (immutable borrow) before we mutably borrow
    // the destination store.
    let mut cloned: Option<(crate::shared::Profile, String)> = None;
    if let Some(src_id) = clone_from {
        let src_store = locks.profile_stores.get_profile_store(&dev, &src_id)?;
        cloned = Some((src_store.value.clone(), src_id));
    }

    let store = locks.profile_stores.get_profile_store_mut(&dev, &id).await?;

    if let Some((mut profile, src_id)) = cloned {
        let old_images_dir = crate::shared::config_dir()
            .join("images")
            .join(&device)
            .join(&src_id);
        let new_images_dir = crate::shared::config_dir()
            .join("images")
            .join(&device)
            .join(&id);

        // Best-effort copy of per-profile image assets (custom icons, etc.).
        if old_images_dir.exists() {
            let _ = std::fs::create_dir_all(&new_images_dir);
            let _ = crate::shared::copy_dir(&old_images_dir, &new_images_dir);
        }

        profile.id = id.clone();

        fn rewrite_instance(
            inst: &mut crate::shared::ActionInstance,
            new_profile: &str,
            old_images_dir: &std::path::Path,
            new_images_dir: &std::path::Path,
        ) {
            inst.context.profile = new_profile.to_owned();
            for state in inst.states.iter_mut() {
                let p = Path::new(state.image.trim());
                if p.starts_with(old_images_dir) {
                    if let Ok(rel) = p.strip_prefix(old_images_dir) {
                        state.image = new_images_dir.join(rel).to_string_lossy().into_owned();
                    }
                }
            }
            if let Some(children) = inst.children.as_mut() {
                for child in children.iter_mut() {
                    rewrite_instance(child, new_profile, old_images_dir, new_images_dir);
                }
            }
        }

        for slot in profile.keys.iter_mut().chain(profile.sliders.iter_mut()) {
            if let Some(inst) = slot.as_mut() {
                rewrite_instance(inst, &id, &old_images_dir, &new_images_dir);
            }
        }

        if let Some(bg) = profile.encoder_screen_background.as_mut() {
            let p = Path::new(bg.trim());
            if p.starts_with(&old_images_dir) {
                if let Ok(rel) = p.strip_prefix(&old_images_dir) {
                    *bg = new_images_dir.join(rel).to_string_lossy().into_owned();
                }
            }
        }

        store.value = profile;
    }

    store.save()?;
    Ok(())
}

pub async fn get_selected_profile(device: String) -> Result<crate::shared::Profile, anyhow::Error> {
    let mut locks = acquire_locks_mut().await;
    if !DEVICES.contains_key(&device) {
        return Err(anyhow::anyhow!("device {device} not found"));
    }

    let selected_profile = locks.device_stores.get_selected_profile(&device)?;
    let profile = locks
        .profile_stores
        .get_profile_store(&DEVICES.get(&device).unwrap(), &selected_profile)?;
    Ok(profile.value.clone())
}

pub async fn set_encoder_screen_background(
    device: String,
    profile: String,
    source_path: Option<String>,
) -> Result<(), anyhow::Error> {
    use std::path::Path;

    let mut locks = acquire_locks_mut().await;
    if !DEVICES.contains_key(&device) {
        return Err(anyhow::anyhow!("device {device} not found"));
    }

    let store = locks
        .profile_stores
        .get_profile_store_mut(&DEVICES.get(&device).unwrap(), &profile)
        .await?;

    let new_value = if let Some(src) = source_path {
        let srcp = Path::new(src.trim());
        if !srcp.is_file() {
            return Err(anyhow::anyhow!("background path not found"));
        }
        let ext = srcp
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("png")
            .to_lowercase();
        let ext = match ext.as_str() {
            "png" | "jpg" | "jpeg" => ext,
            _ => return Err(anyhow::anyhow!("unsupported image type (use png/jpg/jpeg)")),
        };

        let dst_dir = crate::shared::config_dir()
            .join("images")
            .join(&device)
            .join(&profile);
        tokio::fs::create_dir_all(&dst_dir).await?;
        let dst = dst_dir.join(format!("encoder_screen_background.{ext}"));
        tokio::fs::copy(srcp, &dst).await?;
        Some(dst.to_string_lossy().into_owned())
    } else {
        None
    };

    store.value.encoder_screen_background = new_value.clone();
    store.save()?;

    // If this profile is active, apply immediately to the device.
    let active = locks
        .device_stores
        .get_selected_profile(&device)
        .ok()
        .is_some_and(|p| p == profile);
    if active {
        let _ = crate::elgato::set_lcd_background(&device, new_value.as_deref()).await;
        // Re-apply encoder images on top of the background (Plus LCD).
        for instance in store.value.sliders.iter().flatten() {
            let img = instance
                .states
                .get(instance.current_state as usize)
                .map(|s| s.image.trim())
                .filter(|s| !s.is_empty() && *s != "actionDefaultImage")
                .map(|s| s.to_owned())
                .unwrap_or_else(|| instance.action.icon.clone());
            let _ = crate::events::outbound::devices::update_image((&instance.context).into(), Some(img)).await;
        }
    }

    Ok(())
}

#[allow(clippy::flat_map_identity)]
pub async fn set_selected_profile(device: String, id: String) -> Result<(), anyhow::Error> {
    let mut locks = acquire_locks_mut().await;
    if !DEVICES.contains_key(&device) {
        return Err(anyhow::anyhow!("device {device} not found"));
    }

    let selected_profile = locks.device_stores.get_selected_profile(&device)?;

    if selected_profile != id {
        let old_profile = &locks
            .profile_stores
            .get_profile_store(&DEVICES.get(&device).unwrap(), &selected_profile)?
            .value;
        for instance in old_profile
            .keys
            .iter()
            .flatten()
            .chain(old_profile.sliders.iter().flatten())
        {
            if !(crate::shared::is_multi_action_uuid(instance.action.uuid.as_str())
                || crate::shared::is_toggle_action_uuid(instance.action.uuid.as_str()))
            {
                let _ = crate::events::outbound::will_appear::will_disappear(instance, false).await;
            } else {
                for child in instance.children.as_ref().unwrap() {
                    let _ =
                        crate::events::outbound::will_appear::will_disappear(child, false).await;
                }
            }
        }
        let _ = crate::events::outbound::devices::clear_screen(device.clone()).await;
    }

    // We must use the mutable version of get_profile_store in order to create the store if it does not exist.
    let store = locks
        .profile_stores
        .get_profile_store_mut(&DEVICES.get(&device).unwrap(), &id)
        .await?;
    let new_profile = &store.value;

    // If this profile has an encoder LCD background (Stream Deck+), apply it before `willAppear`
    // so per-dial images render on top.
    if let Some(bg) = new_profile.encoder_screen_background.as_deref() {
        let _ = crate::elgato::set_lcd_background(&device, Some(bg)).await;
    }
    for instance in new_profile
        .keys
        .iter()
        .flatten()
        .chain(new_profile.sliders.iter().flatten())
    {
        if !(crate::shared::is_multi_action_uuid(instance.action.uuid.as_str())
            || crate::shared::is_toggle_action_uuid(instance.action.uuid.as_str()))
        {
            let _ = crate::events::outbound::will_appear::will_appear(instance).await;
        } else {
            for child in instance.children.as_ref().unwrap() {
                let _ = crate::events::outbound::will_appear::will_appear(child).await;
            }
        }
    }
    store.save()?;

    locks.device_stores.set_selected_profile(&device, id)?;
    Ok(())
}

pub async fn delete_profile(device: String, profile: String) {
    let mut profile_stores = PROFILE_STORES.write().await;
    profile_stores.delete_profile(&device, &profile);
}

pub async fn rename_profile(
    device: String,
    old_id: String,
    new_id: String,
) -> Result<(), anyhow::Error> {
    let mut locks = acquire_locks_mut().await;
    if !DEVICES.contains_key(&device) {
        return Err(anyhow::anyhow!("device {device} not found"));
    }

    locks
        .profile_stores
        .rename_profile(&DEVICES.get(&device).unwrap(), &old_id, &new_id)
        .await?;
    Ok(())
}
