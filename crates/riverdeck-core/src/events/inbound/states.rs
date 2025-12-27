use super::ContextAndPayloadEvent;

use crate::shared::DEVICES;
use crate::store::profiles::{acquire_locks_mut, get_instance_mut, save_profile};
use crate::ui::{self, UiEvent};

use serde::Deserialize;
use std::path::Component;

#[derive(Deserialize)]
pub struct SetTitlePayload {
    title: Option<String>,
    state: Option<u16>,
}

#[derive(Deserialize)]
pub struct SetImagePayload {
    image: Option<String>,
    state: Option<u16>,
}

#[derive(Deserialize)]
pub struct SetStatePayload {
    state: u16,
}

pub async fn set_title(
    event: ContextAndPayloadEvent<SetTitlePayload>,
) -> Result<(), anyhow::Error> {
    let mut locks = acquire_locks_mut().await;
    let active_profile = locks
        .device_stores
        .get_selected_profile(&event.context.device)
        .ok()
        .is_some_and(|p| p == event.context.profile);
    let active_page = if active_profile {
        locks
            .profile_stores
            .get_profile_store_mut(
                &DEVICES.get(&event.context.device).unwrap(),
                &event.context.profile,
            )
            .await?
            .value
            .selected_page
            .clone()
    } else {
        String::new()
    };

    if let Some(instance) = get_instance_mut(&event.context, &mut locks).await? {
        let mut affects_visible = true;
        if let Some(state) = event.payload.state {
            let Some(slot) = instance.states.get_mut(state as usize) else {
                // Avoid panicking on malformed/out-of-range state indices.
                return Ok(());
            };
            let fallback = instance
                .action
                .states
                .get(state as usize)
                .map(|s| s.text.clone())
                .unwrap_or_default();
            slot.text = event.payload.title.unwrap_or(fallback);
            affects_visible = state == instance.current_state;
        } else {
            for (index, state) in instance.states.iter_mut().enumerate() {
                state.text = event
                    .payload
                    .title
                    .clone()
                    .unwrap_or(instance.action.states[index].text.clone());
            }
        }
        ui::emit(UiEvent::ActionStateChanged {
            context: instance.context.clone(),
        });

        // Repaint hardware immediately so dynamic titles (e.g., System Vitals) show up even if the
        // plugin doesn't call `setImage`.
        let apply_active =
            active_profile && active_page == instance.context.page && affects_visible;
        if apply_active {
            let img = instance
                .states
                .get(instance.current_state as usize)
                .map(|s| s.image.trim())
                .filter(|s| !s.is_empty() && *s != "actionDefaultImage")
                .map(|s| s.to_owned())
                .unwrap_or_else(|| instance.action.icon.clone());
            let _ = crate::events::outbound::devices::update_image(
                (&instance.context).into(),
                if img.trim().is_empty() {
                    None
                } else {
                    Some(img)
                },
            )
            .await;
        }
    }
    save_profile(&event.context.device, &mut locks).await?;

    Ok(())
}

pub async fn set_image(
    mut event: ContextAndPayloadEvent<SetImagePayload>,
) -> Result<(), anyhow::Error> {
    let mut locks = acquire_locks_mut().await;
    // Compute before we take a mutable borrow into `locks` for the instance.
    let active = locks
        .device_stores
        .get_selected_profile(&event.context.device)
        .ok()
        .is_some_and(|p| p == event.context.profile);

    if let Some(instance) = get_instance_mut(&event.context, &mut locks).await? {
        if let Some(image) = &event.payload.image {
            if image.trim().is_empty() {
                event.payload.image = None;
            } else if !image.trim().starts_with("data:") {
                // Treat plugin-provided image paths as *relative to the plugin directory* and
                // reject absolute paths / traversal.
                let rel = std::path::Path::new(image.trim());
                let is_unsafe = rel.is_absolute()
                    || rel.components().any(|c| matches!(c, Component::ParentDir));
                if is_unsafe {
                    event.payload.image = None;
                } else if let Some(p) = crate::shared::config_dir()
                    .join("plugins")
                    .join(&instance.action.plugin)
                    .join(rel)
                    .to_str()
                    .map(|s| s.to_owned())
                {
                    event.payload.image = Some(crate::shared::convert_icon(p));
                } else {
                    event.payload.image = None;
                }
            }
        }

        if let Some(state) = event.payload.state {
            let Some(slot) = instance.states.get_mut(state as usize) else {
                return Ok(());
            };
            let fallback = instance
                .action
                .states
                .get(state as usize)
                .map(|s| s.image.clone())
                .unwrap_or_else(|| instance.action.icon.clone());
            slot.image = event.payload.image.unwrap_or(fallback);
        } else {
            for (index, state) in instance.states.iter_mut().enumerate() {
                state.image = event
                    .payload
                    .image
                    .clone()
                    .unwrap_or(instance.action.states[index].image.clone());
            }
        }
        ui::emit(UiEvent::ActionStateChanged {
            context: instance.context.clone(),
        });

        // Stream Deck SDK: if `state` is provided, only that state's image should change.
        // We should only repaint the physical device if the *visible* state is affected.
        let affects_visible = match event.payload.state {
            None => true,
            Some(s) => s == instance.current_state,
        };

        if active && affects_visible {
            let img = instance
                .states
                .get(instance.current_state as usize)
                .map(|s| s.image.trim())
                .filter(|s| !s.is_empty() && *s != "actionDefaultImage")
                .map(|s| s.to_owned())
                .unwrap_or_else(|| instance.action.icon.clone());

            if let Err(error) = crate::events::outbound::devices::update_image(
                (&instance.context).into(),
                if img.trim().is_empty() {
                    None
                } else {
                    Some(img)
                },
            )
            .await
            {
                log::warn!("Failed to update device image after setImage: {}", error);
            }
        }
    }
    save_profile(&event.context.device, &mut locks).await?;

    Ok(())
}

pub async fn set_state(
    event: ContextAndPayloadEvent<SetStatePayload>,
) -> Result<(), anyhow::Error> {
    let mut locks = acquire_locks_mut().await;
    // Compute before we take a mutable borrow into `locks` for the instance.
    let active = locks
        .device_stores
        .get_selected_profile(&event.context.device)
        .ok()
        .is_some_and(|p| p == event.context.profile);

    if let Some(instance) = get_instance_mut(&event.context, &mut locks).await? {
        if event.payload.state >= instance.states.len() as u16 {
            return Ok(());
        }
        instance.current_state = event.payload.state;
        ui::emit(UiEvent::ActionStateChanged {
            context: instance.context.clone(),
        });

        if active {
            let img = instance
                .states
                .get(instance.current_state as usize)
                .map(|s| s.image.trim())
                .filter(|s| !s.is_empty() && *s != "actionDefaultImage")
                .map(|s| s.to_owned())
                .unwrap_or_else(|| instance.action.icon.clone());
            if let Err(error) = crate::events::outbound::devices::update_image(
                (&instance.context).into(),
                if img.trim().is_empty() {
                    None
                } else {
                    Some(img)
                },
            )
            .await
            {
                log::warn!("Failed to update device image after setState: {}", error);
            }
        }
    }
    save_profile(&event.context.device, &mut locks).await?;

    Ok(())
}
