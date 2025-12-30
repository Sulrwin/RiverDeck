use super::PayloadEvent;

use crate::plugins::DEVICE_NAMESPACES;
use crate::shared::DEVICES;
use crate::store::profiles::get_device_profiles;
use crate::ui::{self, UiEvent};

use serde::Deserialize;

pub async fn register_device(
    uuid: &str,
    mut event: PayloadEvent<crate::shared::DeviceInfo>,
) -> Result<(), anyhow::Error> {
    if uuid.is_empty()
        || Some(uuid)
            == DEVICE_NAMESPACES
                .read()
                .await
                .get(&event.payload.id[..2])
                .map(|x| x.as_str())
    {
        if let Ok(profiles) = get_device_profiles(&event.payload.id) {
            let mut profile_stores = crate::store::profiles::PROFILE_STORES.write().await;
            for profile in profiles {
                // This is called to initialise the store for each profile when the device is registered.
                if let Err(e) = profile_stores
                    .get_profile_store_mut(&event.payload, &profile)
                    .await
                {
                    log::error!("{}", e);
                }
            }
        }

        event.payload.plugin = uuid.to_owned();
        let _ = crate::events::outbound::devices::device_did_connect(
            &event.payload.id,
            (&event.payload).into(),
        )
        .await;
        DEVICES.insert(event.payload.id.clone(), event.payload.clone());
        ui::emit(UiEvent::DevicesUpdated);

        let mut locks = crate::store::profiles::acquire_locks_mut().await;
        let selected_profile = locks
            .device_stores
            .get_selected_profile(&event.payload.id)?;
        let profile = locks
            .profile_stores
            .get_profile_store(&DEVICES.get(&event.payload.id).unwrap(), &selected_profile)?;
        let page = profile
            .value
            .pages
            .iter()
            .find(|p| p.id == profile.value.selected_page)
            .or_else(|| profile.value.pages.first());

        // Stream Deck+ LCD: apply the stored background before `willAppear` so per-dial images overlay it.
        if event.payload.screen.as_ref().is_some_and(|s| {
            matches!(
                s.placement,
                crate::shared::ScreenPlacement::BetweenKeypadAndEncoders
            )
        }) && let Some(page) = page
            && let Some(bg) = page.encoder_screen_background.as_deref()
        {
            let _ = crate::elgato::set_lcd_background_with_crop(
                &event.payload.id,
                Some(bg),
                page.encoder_screen_crop,
            )
            .await;
        }
        if let Some(page) = page {
            for instance in page
                .keys
                .iter()
                .flatten()
                .chain(page.sliders.iter().flatten())
            {
                if !(crate::shared::is_multi_action_uuid(instance.action.uuid.as_str())
                    || crate::shared::is_toggle_action_uuid(instance.action.uuid.as_str()))
                {
                    let _ = crate::events::outbound::will_appear::will_appear(instance).await;
                } else {
                    // Multi/Toggle parent instances are host-side (no real plugin process).
                    // Don't require `send_to_plugin` to succeed; still push a visible icon now.
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
    } else {
        Err(anyhow::anyhow!(
            "plugin {uuid} is not registered for device namespace {}",
            &event.payload.id[..2]
        ))
    }
}

pub async fn deregister_device(
    uuid: &str,
    event: PayloadEvent<String>,
) -> Result<(), anyhow::Error> {
    if uuid.is_empty()
        || Some(uuid)
            == DEVICE_NAMESPACES
                .read()
                .await
                .get(&event.payload[..2])
                .map(|x| x.as_str())
    {
        if !DEVICES.contains_key(&event.payload) {
            return Ok(());
        }

        let mut locks = crate::store::profiles::acquire_locks_mut().await;

        let selected_profile = locks.device_stores.get_selected_profile(&event.payload)?;
        let profile = locks
            .profile_stores
            .get_profile_store(&DEVICES.get(&event.payload).unwrap(), &selected_profile)?;
        let page = profile
            .value
            .pages
            .iter()
            .find(|p| p.id == profile.value.selected_page)
            .or_else(|| profile.value.pages.first());
        if let Some(page) = page {
            for instance in page
                .keys
                .iter()
                .flatten()
                .chain(page.sliders.iter().flatten())
            {
                let _ = crate::events::outbound::will_appear::will_disappear(instance, false).await;
            }
        }

        if let Ok(profiles) = get_device_profiles(&event.payload) {
            for profile in profiles {
                locks
                    .profile_stores
                    .remove_profile(&event.payload, &profile);
            }
        }

        drop(locks);

        let _ = crate::events::outbound::devices::device_did_disconnect(&event.payload).await;
        DEVICES.remove(&event.payload);
        ui::emit(UiEvent::DevicesUpdated);

        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "plugin {uuid} is not registered for device namespace {}",
            &event.payload[..2]
        ))
    }
}

#[derive(Deserialize)]
pub struct PressPayload {
    pub device: String,
    pub position: u8,
}

pub async fn key_down(event: PayloadEvent<PressPayload>) -> Result<(), anyhow::Error> {
    crate::events::outbound::keypad::key_down(&event.payload.device, event.payload.position).await
}

pub async fn key_up(event: PayloadEvent<PressPayload>) -> Result<(), anyhow::Error> {
    crate::events::outbound::keypad::key_up(&event.payload.device, event.payload.position).await
}

#[derive(Deserialize)]
pub struct TicksPayload {
    pub device: String,
    pub position: u8,
    pub ticks: i16,
}

pub async fn encoder_change(event: PayloadEvent<TicksPayload>) -> Result<(), anyhow::Error> {
    crate::events::outbound::encoder::dial_rotate(
        &event.payload.device,
        event.payload.position,
        event.payload.ticks,
    )
    .await
}

pub async fn encoder_down(event: PayloadEvent<PressPayload>) -> Result<(), anyhow::Error> {
    crate::events::outbound::encoder::dial_press(
        &event.payload.device,
        "dialDown",
        event.payload.position,
    )
    .await
}

pub async fn encoder_up(event: PayloadEvent<PressPayload>) -> Result<(), anyhow::Error> {
    crate::events::outbound::encoder::dial_press(
        &event.payload.device,
        "dialUp",
        event.payload.position,
    )
    .await
}

pub async fn rerender_images(event: PayloadEvent<String>) -> Result<(), anyhow::Error> {
    ui::emit(UiEvent::RerenderImages {
        device: event.payload,
    });
    Ok(())
}
