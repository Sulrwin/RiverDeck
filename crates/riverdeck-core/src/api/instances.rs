use crate::shared::{Action, ActionContext, ActionInstance, Context, config_dir};
use crate::store::profiles::{acquire_locks_mut, get_slot_mut, save_profile};
use crate::ui::{self, UiEvent};

use tokio::fs::remove_dir_all;

pub async fn create_instance(
    action: Action,
    context: Context,
) -> Result<Option<ActionInstance>, anyhow::Error> {
    if !action.controllers.contains(&context.controller) {
        return Ok(None);
    }

    let mut locks = acquire_locks_mut().await;
    let slot = get_slot_mut(&context, &mut locks).await?;

    if let Some(parent) = slot {
        let Some(children) = &mut parent.children else {
            return Ok(None);
        };
        let index = match children.last() {
            None => 1,
            Some(instance) => instance.context.index + 1,
        };

        let instance = ActionInstance {
            action: action.clone(),
            context: ActionContext::from_context(context.clone(), index),
            states: action.states.clone(),
            current_state: 0,
            settings: serde_json::Value::Object(serde_json::Map::new()),
            children: None,
        };
        children.push(instance.clone());

        if matches!(
            parent.action.uuid.as_str(),
            "riverdeck.toggleaction" | "opendeck.toggleaction"
        ) && parent.states.len() < children.len()
        {
            parent.states.push(crate::shared::ActionState {
                image: "riverdeck/toggle-action.png".to_owned(),
                ..Default::default()
            });
            ui::emit(UiEvent::ActionStateChanged {
                context: parent.context.clone(),
            });
        }

        save_profile(&context.device, &mut locks).await?;
        let _ = crate::events::outbound::will_appear::will_appear(&instance).await;

        Ok(Some(instance))
    } else {
        let instance = ActionInstance {
            action: action.clone(),
            context: ActionContext::from_context(context.clone(), 0),
            states: action.states.clone(),
            current_state: 0,
            settings: serde_json::Value::Object(serde_json::Map::new()),
            children: if matches!(
                action.uuid.as_str(),
                "riverdeck.multiaction"
                    | "riverdeck.toggleaction"
                    | "opendeck.multiaction"
                    | "opendeck.toggleaction"
            ) {
                Some(vec![])
            } else {
                None
            },
        };

        *slot = Some(instance.clone());
        let slot = slot.clone();

        save_profile(&context.device, &mut locks).await?;
        let _ = crate::events::outbound::will_appear::will_appear(&instance).await;

        Ok(slot)
    }
}

fn instance_images_dir(context: &ActionContext) -> std::path::PathBuf {
    config_dir()
        .join("images")
        .join(&context.device)
        .join(&context.profile)
        .join(format!(
            "{}.{}.{}",
            context.controller, context.position, context.index
        ))
}

pub async fn move_instance(
    source: Context,
    destination: Context,
    retain: bool,
) -> Result<Option<ActionInstance>, anyhow::Error> {
    if source.controller != destination.controller {
        return Ok(None);
    }

    {
        let locks = crate::store::profiles::acquire_locks().await;
        let dst = crate::store::profiles::get_slot(&destination, &locks).await?;
        if dst.is_some() {
            return Ok(None);
        }
    }

    let mut locks = acquire_locks_mut().await;
    let src = get_slot_mut(&source, &mut locks).await?;

    let Some(mut new) = src.clone() else {
        return Ok(None);
    };
    new.context = ActionContext::from_context(destination.clone(), 0);
    if let Some(children) = &mut new.children {
        for (index, instance) in children.iter_mut().enumerate() {
            instance.context = ActionContext::from_context(destination.clone(), index as u16 + 1);
            for (i, state) in instance.states.iter_mut().enumerate() {
                if !instance.action.states[i].image.is_empty() {
                    state.image = instance.action.states[i].image.clone();
                } else {
                    state.image = instance.action.icon.clone();
                }
            }
        }
    }

    let old_dir = instance_images_dir(&src.as_ref().unwrap().context);
    let new_dir = instance_images_dir(&new.context);
    let _ = tokio::fs::create_dir_all(&new_dir).await;
    if let Ok(files) = old_dir.read_dir() {
        for file in files.flatten() {
            let _ = tokio::fs::copy(file.path(), new_dir.join(file.file_name())).await;
        }
    }
    for state in new.states.iter_mut() {
        let path = std::path::Path::new(&state.image);
        if path.starts_with(&old_dir) {
            state.image = new_dir
                .join(path.strip_prefix(&old_dir).unwrap())
                .to_string_lossy()
                .into_owned();
        }
    }

    let dst = get_slot_mut(&destination, &mut locks).await?;
    *dst = Some(new.clone());

    if !retain {
        let src = get_slot_mut(&source, &mut locks).await?;
        if let Some(old) = src {
            let _ = crate::events::outbound::will_appear::will_disappear(old, true).await;
            let _ = remove_dir_all(instance_images_dir(&old.context)).await;
        }
        *src = None;
    }

    let _ = crate::events::outbound::will_appear::will_appear(&new).await;
    save_profile(&destination.device, &mut locks).await?;
    ui::emit(UiEvent::ActionStateChanged {
        context: new.context.clone(),
    });

    Ok(Some(new))
}

pub async fn remove_instance(context: ActionContext) -> Result<(), anyhow::Error> {
    let mut locks = acquire_locks_mut().await;
    let slot = get_slot_mut(&(&context).into(), &mut locks).await?;
    let Some(instance) = slot else {
        return Ok(());
    };

    if instance.context == context {
        let _ = crate::events::outbound::will_appear::will_disappear(instance, true).await;
        if let Some(children) = &instance.children {
            for child in children {
                let _ = crate::events::outbound::will_appear::will_disappear(child, true).await;
                let _ = remove_dir_all(instance_images_dir(&child.context)).await;
            }
        }
        let _ = remove_dir_all(instance_images_dir(&instance.context)).await;
        *slot = None;
    } else {
        let children = instance.children.as_mut().unwrap();
        for (index, instance) in children.iter().enumerate() {
            if instance.context == context {
                let _ = crate::events::outbound::will_appear::will_disappear(instance, true).await;
                let _ = remove_dir_all(instance_images_dir(&instance.context)).await;
                children.remove(index);
                break;
            }
        }
        if matches!(
            instance.action.uuid.as_str(),
            "riverdeck.toggleaction" | "opendeck.toggleaction"
        ) {
            if instance.current_state as usize >= children.len() {
                instance.current_state = if children.is_empty() {
                    0
                } else {
                    children.len() as u16 - 1
                };
            }
            if !children.is_empty() {
                instance.states.pop();
                ui::emit(UiEvent::ActionStateChanged {
                    context: instance.context.clone(),
                });
            }
        }
    }

    save_profile(&context.device, &mut locks).await?;
    Ok(())
}

pub async fn set_state(instance: ActionInstance, state: u16) -> Result<(), anyhow::Error> {
    let mut locks = acquire_locks_mut().await;
    let reference = crate::store::profiles::get_instance_mut(&instance.context, &mut locks)
        .await?
        .unwrap();
    *reference = instance.clone();
    save_profile(&instance.context.device, &mut locks).await?;
    crate::events::outbound::states::title_parameters_did_change(&instance, state).await?;
    ui::emit(UiEvent::ActionStateChanged {
        context: instance.context,
    });
    Ok(())
}

pub async fn update_image(context: Context, image: String) {
    if Some(&context.profile)
        != crate::store::profiles::DEVICE_STORES
            .write()
            .await
            .get_selected_profile(&context.device)
            .ok()
            .as_ref()
    {
        return;
    }

    if let Err(error) = crate::events::outbound::devices::update_image(context, Some(image)).await {
        log::warn!("Failed to update device image: {}", error);
    }
}
