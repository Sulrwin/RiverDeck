use super::{GenericInstancePayload, send_to_plugin};

use crate::shared::{ActionContext, ActionInstance, Context, DEVICES};
use crate::store::profiles::{acquire_locks_mut, get_slot_mut, save_profile};
use crate::ui::{self, UiEvent};

use std::time::Duration;

use serde::Serialize;

fn multi_action_run_on(settings: &serde_json::Value) -> &'static str {
    match settings
        .get("runOn")
        .and_then(|v| v.as_str())
        .unwrap_or("keyDown")
    {
        "keyUp" => "keyUp",
        _ => "keyDown",
    }
}

async fn execute_multi_action(
    instance: &mut ActionInstance,
) -> Result<Vec<ActionContext>, anyhow::Error> {
    let Some(children) = instance.children.as_mut() else {
        return Ok(vec![]);
    };

    for child in children.iter_mut() {
        send_to_plugin(
            &child.action.plugin,
            &KeyEvent {
                event: "keyDown",
                action: child.action.uuid.clone(),
                context: child.context.clone(),
                device: child.context.device.clone(),
                payload: GenericInstancePayload::new(child),
            },
        )
        .await?;

        tokio::time::sleep(Duration::from_millis(100)).await;

        if child.states.len() == 2 && !child.action.disable_automatic_states {
            child.current_state = (child.current_state + 1) % (child.states.len() as u16);
        }

        send_to_plugin(
            &child.action.plugin,
            &KeyEvent {
                event: "keyUp",
                action: child.action.uuid.clone(),
                context: child.context.clone(),
                device: child.context.device.clone(),
                payload: GenericInstancePayload::new(child),
            },
        )
        .await?;

        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    Ok(children.iter().map(|c| c.context.clone()).collect())
}

#[derive(Serialize)]
struct KeyEvent {
    event: &'static str,
    action: String,
    context: ActionContext,
    device: String,
    payload: GenericInstancePayload,
}

pub async fn key_down(device: &str, key: u8) -> Result<(), anyhow::Error> {
    let mut locks = acquire_locks_mut().await;
    let selected_profile = locks.device_stores.get_selected_profile(device)?;
    let page = locks
        .profile_stores
        .get_profile_store_mut(&DEVICES.get(device).unwrap(), &selected_profile)
        .await?
        .value
        .selected_page
        .clone();
    let context = Context {
        device: device.to_owned(),
        profile: selected_profile.to_owned(),
        page,
        controller: "Keypad".to_owned(),
        position: key,
    };

    ui::emit(UiEvent::KeyMoved {
        context: context.clone(),
        down: true,
    });

    let Some(instance) = get_slot_mut(&context, &mut locks).await? else {
        return Ok(());
    };
    if crate::shared::is_multi_action_uuid(instance.action.uuid.as_str()) {
        if multi_action_run_on(&instance.settings) == "keyDown" {
            let contexts = execute_multi_action(instance).await?;

            for child in contexts {
                ui::emit(UiEvent::ActionStateChanged { context: child });
            }

            save_profile(device, &mut locks).await?;
        }
    } else if crate::shared::is_toggle_action_uuid(instance.action.uuid.as_str()) {
        let children = instance.children.as_ref().unwrap();
        if children.is_empty() {
            return Ok(());
        }
        let child = &children[instance.current_state as usize];
        send_to_plugin(
            &child.action.plugin,
            &KeyEvent {
                event: "keyDown",
                action: child.action.uuid.clone(),
                context: child.context.clone(),
                device: child.context.device.clone(),
                payload: GenericInstancePayload::new(child),
            },
        )
        .await?;
    } else {
        send_to_plugin(
            &instance.action.plugin,
            &KeyEvent {
                event: "keyDown",
                action: instance.action.uuid.clone(),
                context: instance.context.clone(),
                device: instance.context.device.clone(),
                payload: GenericInstancePayload::new(instance),
            },
        )
        .await?;
    }

    Ok(())
}

pub async fn key_up(device: &str, key: u8) -> Result<(), anyhow::Error> {
    let mut locks = acquire_locks_mut().await;
    let selected_profile = locks.device_stores.get_selected_profile(device)?;
    let page = locks
        .profile_stores
        .get_profile_store_mut(&DEVICES.get(device).unwrap(), &selected_profile)
        .await?
        .value
        .selected_page
        .clone();
    let context = Context {
        device: device.to_owned(),
        profile: selected_profile.to_owned(),
        page,
        controller: "Keypad".to_owned(),
        position: key,
    };

    ui::emit(UiEvent::KeyMoved {
        context: context.clone(),
        down: false,
    });

    let slot = get_slot_mut(&context, &mut locks).await?;
    let Some(instance) = slot else { return Ok(()) };

    if crate::shared::is_toggle_action_uuid(instance.action.uuid.as_str()) {
        let index = instance.current_state as usize;
        let children = instance.children.as_ref().unwrap();
        if children.is_empty() {
            return Ok(());
        }
        let child = &children[index];
        send_to_plugin(
            &child.action.plugin,
            &KeyEvent {
                event: "keyUp",
                action: child.action.uuid.clone(),
                context: child.context.clone(),
                device: child.context.device.clone(),
                payload: GenericInstancePayload::new(child),
            },
        )
        .await?;
        instance.current_state = ((index + 1) % instance.children.as_ref().unwrap().len()) as u16;
    } else if crate::shared::is_multi_action_uuid(instance.action.uuid.as_str()) {
        if multi_action_run_on(&instance.settings) == "keyUp" {
            let contexts = execute_multi_action(instance).await?;
            for child in contexts {
                ui::emit(UiEvent::ActionStateChanged { context: child });
            }
        }
    } else {
        if instance.states.len() == 2 && !instance.action.disable_automatic_states {
            instance.current_state = (instance.current_state + 1) % (instance.states.len() as u16);
        }
        send_to_plugin(
            &instance.action.plugin,
            &KeyEvent {
                event: "keyUp",
                action: instance.action.uuid.clone(),
                context: instance.context.clone(),
                device: instance.context.device.clone(),
                payload: GenericInstancePayload::new(instance),
            },
        )
        .await?;
    };

    ui::emit(UiEvent::ActionStateChanged {
        context: instance.context.clone(),
    });
    save_profile(device, &mut locks).await?;

    Ok(())
}
