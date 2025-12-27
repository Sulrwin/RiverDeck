use super::ContextAndPayloadEvent;

use crate::store::profiles::{acquire_locks_mut, get_instance_mut, save_profile};
use crate::ui::{self, UiEvent};

use serde::Deserialize;

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

    if let Some(instance) = get_instance_mut(&event.context, &mut locks).await? {
        if let Some(state) = event.payload.state {
            instance.states[state as usize].text = event
                .payload
                .title
                .unwrap_or(instance.action.states[state as usize].text.clone());
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
                event.payload.image = Some(crate::shared::convert_icon(
                    crate::shared::config_dir()
                        .join("plugins")
                        .join(&instance.action.plugin)
                        .join(image.trim())
                        .to_str()
                        .unwrap()
                        .to_owned(),
                ));
            }
        }

        if let Some(state) = event.payload.state {
            instance.states[state as usize].image = event
                .payload
                .image
                .unwrap_or(instance.action.states[state as usize].image.clone());
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
