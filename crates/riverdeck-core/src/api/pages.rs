use crate::shared::{DEVICES, Page, Profile};
use crate::store::profiles::acquire_locks_mut;

fn page_index(profile: &Profile, page_id: &str) -> Option<usize> {
    profile.pages.iter().position(|p| p.id == page_id)
}

fn selected_page(profile: &Profile) -> Option<&Page> {
    profile
        .pages
        .iter()
        .find(|p| p.id == profile.selected_page)
        .or_else(|| profile.pages.first())
}

fn selected_page_mut(profile: &mut Profile) -> &mut Page {
    if profile.pages.is_empty() {
        profile.pages.push(Page {
            id: "1".to_owned(),
            keys: vec![],
            sliders: vec![],
            encoder_screen_background: None,
            encoder_screen_crop: None,
        });
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

pub async fn get_pages(device: String, profile: String) -> Result<Vec<String>, anyhow::Error> {
    let locks = crate::store::profiles::acquire_locks().await;
    if !DEVICES.contains_key(&device) {
        return Err(anyhow::anyhow!("device {device} not found"));
    }
    let dev = DEVICES.get(&device).unwrap();
    let store = locks.profile_stores.get_profile_store(&dev, &profile)?;
    Ok(store.value.pages.iter().map(|p| p.id.clone()).collect())
}

pub async fn get_selected_page(device: String, profile: String) -> Result<String, anyhow::Error> {
    let mut locks = acquire_locks_mut().await;
    if !DEVICES.contains_key(&device) {
        return Err(anyhow::anyhow!("device {device} not found"));
    }
    let store = locks
        .profile_stores
        .get_profile_store_mut(&DEVICES.get(&device).unwrap(), &profile)
        .await?;
    Ok(store.value.selected_page.clone())
}

pub async fn set_selected_page(
    device: String,
    profile_id: String,
    page_id: String,
) -> Result<(), anyhow::Error> {
    let mut locks = acquire_locks_mut().await;
    if !DEVICES.contains_key(&device) {
        return Err(anyhow::anyhow!("device {device} not found"));
    }

    // Ensure profile store exists.
    let dev = DEVICES.get(&device).unwrap();
    let store = locks
        .profile_stores
        .get_profile_store_mut(&dev, &profile_id)
        .await?;

    // Create page if missing (best effort).
    if page_index(&store.value, &page_id).is_none() {
        store.value.pages.push(Page {
            id: page_id.clone(),
            keys: vec![],
            sliders: vec![],
            encoder_screen_background: None,
            encoder_screen_crop: None,
        });
    }

    let old_page_id = store.value.selected_page.clone();
    if old_page_id == page_id {
        return Ok(());
    }

    // Only do willDisappear/Appear + device updates if this profile is active for the device.
    let active_profile = locks.device_stores.get_selected_profile(&device)?;
    let is_active_profile = active_profile == profile_id;

    if is_active_profile {
        if let Some(old_page) = selected_page(&store.value) {
            for instance in old_page
                .keys
                .iter()
                .flatten()
                .chain(old_page.sliders.iter().flatten())
            {
                if !(crate::shared::is_multi_action_uuid(instance.action.uuid.as_str())
                    || crate::shared::is_toggle_action_uuid(instance.action.uuid.as_str()))
                {
                    let _ =
                        crate::events::outbound::will_appear::will_disappear(instance, false).await;
                } else if let Some(children) = instance.children.as_ref() {
                    for child in children {
                        let _ = crate::events::outbound::will_appear::will_disappear(child, false)
                            .await;
                    }
                }
            }
        }
        let _ = crate::events::outbound::devices::clear_screen(device.clone()).await;
    }

    // Switch selected page and save.
    store.value.selected_page = page_id.clone();

    // Ensure slots are resized for the device.
    let page = selected_page_mut(&mut store.value);
    page.keys.resize((dev.rows * dev.columns) as usize, None);
    page.sliders.resize(dev.encoders as usize, None);

    store.save()?;

    if is_active_profile {
        let new_page = selected_page(&store.value).unwrap();

        if let Some(bg) = new_page.encoder_screen_background.as_deref() {
            let _ = crate::elgato::set_lcd_background_with_crop(
                &device,
                Some(bg),
                new_page.encoder_screen_crop,
            )
            .await;
        } else {
            let _ = crate::elgato::set_lcd_background_with_crop(
                &device,
                None,
                new_page.encoder_screen_crop,
            )
            .await;
        }

        for instance in new_page
            .keys
            .iter()
            .flatten()
            .chain(new_page.sliders.iter().flatten())
        {
            if !(crate::shared::is_multi_action_uuid(instance.action.uuid.as_str())
                || crate::shared::is_toggle_action_uuid(instance.action.uuid.as_str()))
            {
                let _ = crate::events::outbound::will_appear::will_appear(instance).await;
            } else {
                // Multi/Toggle parent instances are built-in and don't have a real plugin process
                // behind them, so we avoid queuing `willAppear`. But we *must* still push a visible
                // icon back to the device after `clear_screen()` (e.g. when paging via swipe).
                let _ = crate::events::outbound::devices::update_image_for_instance(
                    instance,
                    crate::events::outbound::devices::effective_image_for_instance(instance),
                )
                .await;

                if let Some(children) = instance.children.as_ref() {
                    for child in children {
                        let _ = crate::events::outbound::will_appear::will_appear(child).await;
                    }
                }
            }
        }
    }

    Ok(())
}

/// Shift the currently selected page for the device's active profile.
///
/// - `delta = 1` selects the next page (wraps at end).
/// - `delta = -1` selects the previous page (wraps at start).
pub async fn shift_selected_page(device: String, delta: i32) -> Result<(), anyhow::Error> {
    if delta == 0 {
        return Ok(());
    }

    // Compute the next page id under a single lock, then drop and route through `set_selected_page`
    // for correct willDisappear/willAppear + device update behavior.
    let (profile_id, next_page_id) = {
        let mut locks = acquire_locks_mut().await;
        if !DEVICES.contains_key(&device) {
            return Err(anyhow::anyhow!("device {device} not found"));
        }
        let profile_id = locks.device_stores.get_selected_profile(&device)?;

        let dev = DEVICES.get(&device).unwrap();
        let store = locks
            .profile_stores
            .get_profile_store_mut(&dev, &profile_id)
            .await?;

        if store.value.pages.len() <= 1 {
            return Ok(());
        }

        let cur_id = store.value.selected_page.clone();
        let cur_idx = store
            .value
            .pages
            .iter()
            .position(|p| p.id == cur_id)
            .unwrap_or(0) as i32;
        let len = store.value.pages.len() as i32;
        let next_idx = (cur_idx + delta).rem_euclid(len) as usize;
        let next_page_id = store
            .value
            .pages
            .get(next_idx)
            .map(|p| p.id.clone())
            .unwrap_or_else(|| "1".to_owned());

        (profile_id, next_page_id)
    };

    set_selected_page(device, profile_id, next_page_id).await
}

pub async fn create_page_and_select(
    device: String,
    profile_id: String,
) -> Result<String, anyhow::Error> {
    let mut locks = acquire_locks_mut().await;
    if !DEVICES.contains_key(&device) {
        return Err(anyhow::anyhow!("device {device} not found"));
    }
    let dev = DEVICES.get(&device).unwrap();
    let store = locks
        .profile_stores
        .get_profile_store_mut(&dev, &profile_id)
        .await?;

    // Pick next numeric page id (starting at 2, because "1" is typical default).
    let mut n: u32 = 2;
    loop {
        let candidate = n.to_string();
        if store.value.pages.iter().all(|p| p.id != candidate) {
            store.value.pages.push(Page {
                id: candidate.clone(),
                keys: vec![None; (dev.rows * dev.columns) as usize],
                sliders: vec![None; dev.encoders as usize],
                encoder_screen_background: None,
                encoder_screen_crop: None,
            });
            store.save()?;
            drop(locks);

            // Route through set_selected_page for proper willDisappear/willAppear behavior if active.
            let _ = set_selected_page(device, profile_id.clone(), candidate.clone()).await;
            return Ok(candidate);
        }
        n += 1;
    }
}

pub async fn delete_page(
    device: String,
    profile_id: String,
    page_id: String,
) -> Result<(), anyhow::Error> {
    // If we are deleting the currently selected page for an active profile, switch to another
    // page first so we can run willDisappear/willAppear against real page data.
    let (must_switch, next_page) = {
        let mut locks = acquire_locks_mut().await;
        if !DEVICES.contains_key(&device) {
            return Err(anyhow::anyhow!("device {device} not found"));
        }
        let dev = DEVICES.get(&device).unwrap();
        let store = locks
            .profile_stores
            .get_profile_store_mut(&dev, &profile_id)
            .await?;

        if store.value.pages.len() <= 1 {
            return Ok(());
        }
        let active_profile = locks.device_stores.get_selected_profile(&device)?;
        let is_active_profile = active_profile == profile_id;
        let was_selected = store.value.selected_page == page_id;
        let next_page = if was_selected {
            store
                .value
                .pages
                .iter()
                .find(|p| p.id != page_id)
                .map(|p| p.id.clone())
                .unwrap_or_else(|| "1".to_owned())
        } else {
            String::new()
        };
        (is_active_profile && was_selected, next_page)
    };
    if must_switch {
        let _ = set_selected_page(device.clone(), profile_id.clone(), next_page).await;
    }

    // Now remove the page from disk/profile.
    let mut locks = acquire_locks_mut().await;
    let dev = DEVICES.get(&device).unwrap();
    let store = locks
        .profile_stores
        .get_profile_store_mut(&dev, &profile_id)
        .await?;
    if store.value.pages.len() <= 1 {
        return Ok(());
    }
    if let Some(idx) = page_index(&store.value, &page_id) {
        store.value.pages.remove(idx);
        if store.value.selected_page == page_id {
            store.value.selected_page = store
                .value
                .pages
                .first()
                .map(|p| p.id.clone())
                .unwrap_or_else(|| "1".to_owned());
        }
        store.save()?;
    }

    let images_dir = crate::shared::config_dir()
        .join("images")
        .join(&device)
        .join(&profile_id)
        .join(&page_id);
    let _ = tokio::fs::remove_dir_all(images_dir).await;

    Ok(())
}
