use crate::shared::{
    Action, ActionContext, ActionInstance, ActionState, Context, DEVICES, FontSize, TextPlacement,
    config_dir,
};
use crate::store::profiles::{acquire_locks_mut, get_slot_mut, save_profile};
use crate::ui::{self, UiEvent};

use tokio::fs::remove_dir_all;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MultiActionRunOn {
    KeyDown,
    KeyUp,
}

impl MultiActionRunOn {
    fn as_str(self) -> &'static str {
        match self {
            Self::KeyDown => "keyDown",
            Self::KeyUp => "keyUp",
        }
    }
}

fn parse_multi_action_run_on(settings: &serde_json::Value) -> MultiActionRunOn {
    match settings
        .get("runOn")
        .and_then(|v| v.as_str())
        .unwrap_or("keyDown")
    {
        "keyUp" => MultiActionRunOn::KeyUp,
        _ => MultiActionRunOn::KeyDown,
    }
}

fn toggle_child_icon_for_state(child: &ActionInstance) -> String {
    let st = child
        .states
        .get(child.current_state as usize)
        .or_else(|| child.states.first());
    if let Some(st) = st {
        let img = st.image.trim();
        if !img.is_empty() && img != "actionDefaultImage" {
            return img.to_owned();
        }
    }
    child.action.icon.clone()
}

fn sync_toggle_state_images_from_children(parent: &mut ActionInstance) {
    if !crate::shared::is_toggle_action_uuid(parent.action.uuid.as_str()) {
        return;
    }
    let Some(children) = parent.children.as_ref() else {
        return;
    };
    let n = children.len();
    if n == 0 {
        parent.current_state = 0;
        // Keep the first state only (matches legacy behavior).
        if parent.states.len() > 1 {
            parent.states.truncate(1);
        }
        return;
    }

    if parent.current_state as usize >= n {
        parent.current_state = (n - 1) as u16;
    }

    // Ensure state count matches child count.
    while parent.states.len() < n {
        parent.states.push(ActionState::default());
    }
    if parent.states.len() > n {
        parent.states.truncate(n);
    }

    for (i, child) in children.iter().enumerate() {
        parent.states[i].image = toggle_child_icon_for_state(child);
    }
}

fn apply_context_for_visuals(ctx: &ActionContext) -> ActionContext {
    if ctx.index == 0 {
        return ctx.clone();
    }
    let base: Context = ctx.into();
    ActionContext::from_context(base, 0)
}

pub async fn create_instance(
    action: Action,
    context: Context,
) -> Result<Option<ActionInstance>, anyhow::Error> {
    let mut action = action;
    crate::shared::normalize_builtin_action(&mut action.plugin, &mut action.uuid);
    crate::shared::normalize_starterpack_action(&mut action.plugin, &mut action.uuid);

    if !action.controllers.contains(&context.controller) {
        return Ok(None);
    }

    let mut locks = acquire_locks_mut().await;
    let slot = get_slot_mut(&context, &mut locks).await?;

    let init_states = |mut states: Vec<ActionState>| -> Vec<ActionState> {
        for st in states.iter_mut() {
            if st.text.trim().is_empty() {
                st.text = action.name.clone();
            }
        }
        states
    };

    if let Some(parent) = slot {
        let Some(children) = &mut parent.children else {
            return Ok(None);
        };
        // When adding a child to a Multi/Toggle action, enforce multi-action constraints.
        if crate::shared::is_multi_action_uuid(action.uuid.as_str())
            || crate::shared::is_toggle_action_uuid(action.uuid.as_str())
        {
            return Ok(None);
        }
        if !action.supported_in_multi_actions {
            return Ok(None);
        }

        let index = match children.last() {
            None => 1,
            Some(instance) => instance.context.index + 1,
        };

        let instance = ActionInstance {
            action: action.clone(),
            context: ActionContext::from_context(context.clone(), index),
            states: init_states(action.states.clone()),
            current_state: 0,
            settings: serde_json::Value::Object(serde_json::Map::new()),
            children: None,
        };
        children.push(instance.clone());

        if crate::shared::is_toggle_action_uuid(parent.action.uuid.as_str()) {
            // Maintain state count (legacy), then sync state images from children.
            if parent.states.len() < children.len() {
                parent.states.push(crate::shared::ActionState::default());
            }
            sync_toggle_state_images_from_children(parent);
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
            states: init_states(action.states.clone()),
            current_state: 0,
            settings: serde_json::Value::Object(serde_json::Map::new()),
            children: if crate::shared::is_multi_action_uuid(action.uuid.as_str())
                || crate::shared::is_toggle_action_uuid(action.uuid.as_str())
            {
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

pub async fn set_multi_action_run_on(
    parent_ctx: ActionContext,
    run_on: MultiActionRunOn,
) -> Result<(), anyhow::Error> {
    let mut locks = acquire_locks_mut().await;
    let device_id = {
        let Some(instance) =
            crate::store::profiles::get_instance_mut(&parent_ctx, &mut locks).await?
        else {
            return Ok(());
        };

        // Only parent instances (index 0) can be Multi Actions.
        if instance.context.index != 0
            || !crate::shared::is_multi_action_uuid(instance.action.uuid.as_str())
        {
            return Ok(());
        }

        let current = parse_multi_action_run_on(&instance.settings);
        if current == run_on {
            return Ok(());
        }

        let settings_obj = match instance.settings.as_object_mut() {
            Some(obj) => obj,
            None => {
                instance.settings = serde_json::Value::Object(serde_json::Map::new());
                instance.settings.as_object_mut().unwrap()
            }
        };
        settings_obj.insert(
            "runOn".to_owned(),
            serde_json::Value::String(run_on.as_str().to_owned()),
        );

        ui::emit(UiEvent::ActionStateChanged {
            context: instance.context.clone(),
        });
        instance.context.device.clone()
    };

    save_profile(&device_id, &mut locks).await?;
    Ok(())
}

/// Host-side helper to update an instance's settings and notify the plugin (like a Property Inspector would).
pub async fn set_instance_settings(
    context: ActionContext,
    settings: serde_json::Value,
) -> Result<(), anyhow::Error> {
    let mut locks = acquire_locks_mut().await;

    if let Some(instance) = crate::store::profiles::get_instance_mut(&context, &mut locks).await? {
        instance.settings = settings;
        // Notify the plugin (not the PI).
        crate::events::outbound::settings::did_receive_settings(instance, false).await?;
        // Best-effort UI refresh for hosts that cache snapshots.
        ui::emit(UiEvent::ActionStateChanged {
            context: instance.context.clone(),
        });
        save_profile(&context.device, &mut locks).await?;
    }

    Ok(())
}

pub async fn reorder_multi_action_child(
    parent_ctx: ActionContext,
    from: usize,
    to: usize,
) -> Result<(), anyhow::Error> {
    if from == to {
        return Ok(());
    }
    let mut locks = acquire_locks_mut().await;
    let device_id = {
        let Some(instance) =
            crate::store::profiles::get_instance_mut(&parent_ctx, &mut locks).await?
        else {
            return Ok(());
        };
        if instance.context.index != 0
            || !crate::shared::is_multi_action_uuid(instance.action.uuid.as_str())
        {
            return Ok(());
        }
        let Some(children) = instance.children.as_mut() else {
            return Ok(());
        };
        if from >= children.len() || to >= children.len() {
            return Ok(());
        }

        // Preserve child contexts; ordering is what matters for execution.
        let child = children.remove(from);
        children.insert(to, child);

        ui::emit(UiEvent::ActionStateChanged {
            context: instance.context.clone(),
        });
        instance.context.device.clone()
    };

    save_profile(&device_id, &mut locks).await?;
    Ok(())
}

pub async fn reorder_toggle_action_child(
    parent_ctx: ActionContext,
    from: usize,
    to: usize,
) -> Result<(), anyhow::Error> {
    if from == to {
        return Ok(());
    }
    let mut locks = acquire_locks_mut().await;
    let device_id = {
        let Some(instance) =
            crate::store::profiles::get_instance_mut(&parent_ctx, &mut locks).await?
        else {
            return Ok(());
        };
        if instance.context.index != 0
            || !crate::shared::is_toggle_action_uuid(instance.action.uuid.as_str())
        {
            return Ok(());
        }
        let Some(children) = instance.children.as_mut() else {
            return Ok(());
        };
        if from >= children.len() || to >= children.len() {
            return Ok(());
        }

        // Keep the same logical current child selected across reorder.
        let current_ctx = children
            .get(instance.current_state as usize)
            .map(|c| c.context.clone());

        // Preserve child contexts; ordering is what matters for execution.
        let child = children.remove(from);
        children.insert(to, child);

        // Keep parent states aligned with children ordering.
        if instance.states.len() == children.len() {
            let st = instance.states.remove(from);
            instance.states.insert(to, st);
        }

        if let Some(cur) = current_ctx
            && let Some(new_idx) = children.iter().position(|c| c.context == cur)
        {
            instance.current_state = new_idx as u16;
        }

        // Now that we are done borrowing `children`, update the parent state images.
        sync_toggle_state_images_from_children(instance);

        ui::emit(UiEvent::ActionStateChanged {
            context: instance.context.clone(),
        });
        instance.context.device.clone()
    };

    save_profile(&device_id, &mut locks).await?;
    Ok(())
}

pub async fn set_button_label(context: ActionContext, label: String) -> Result<(), anyhow::Error> {
    let mut locks = acquire_locks_mut().await;
    let active_profile = locks
        .device_stores
        .get_selected_profile(&context.device)
        .ok()
        .is_some_and(|p| p == context.profile);
    let active_page = if active_profile {
        locks
            .profile_stores
            .get_profile_store_mut(&DEVICES.get(&context.device).unwrap(), &context.profile)
            .await?
            .value
            .selected_page
            .clone()
    } else {
        String::new()
    };
    {
        let Some(instance) = crate::store::profiles::get_instance_mut(&context, &mut locks).await?
        else {
            return Ok(());
        };
        for st in instance.states.iter_mut() {
            st.text = label.clone();
        }
        ui::emit(UiEvent::ActionStateChanged {
            context: instance.context.clone(),
        });
    }

    let apply_ctx = apply_context_for_visuals(&context);
    let (apply_ctx, apply_img, apply_overlays, apply_active) = {
        let Some(instance) =
            crate::store::profiles::get_instance_mut(&apply_ctx, &mut locks).await?
        else {
            return Ok(());
        };
        ui::emit(UiEvent::ActionStateChanged {
            context: instance.context.clone(),
        });
        let img = crate::events::outbound::devices::effective_image_for_instance(instance);
        (
            instance.context.clone(),
            img,
            crate::events::outbound::devices::overlays_for_instance(instance),
            active_profile && active_page == instance.context.page,
        )
    };
    save_profile(&context.device, &mut locks).await?;
    if apply_active {
        let _ = crate::events::outbound::devices::update_image_with_overlays(
            (&apply_ctx).into(),
            apply_img,
            apply_overlays,
        )
        .await;
    }
    Ok(())
}

pub async fn set_button_label_placement(
    context: ActionContext,
    placement: TextPlacement,
) -> Result<(), anyhow::Error> {
    let mut locks = acquire_locks_mut().await;
    let active_profile = locks
        .device_stores
        .get_selected_profile(&context.device)
        .ok()
        .is_some_and(|p| p == context.profile);
    let active_page = if active_profile {
        locks
            .profile_stores
            .get_profile_store_mut(&DEVICES.get(&context.device).unwrap(), &context.profile)
            .await?
            .value
            .selected_page
            .clone()
    } else {
        String::new()
    };
    {
        let Some(instance) = crate::store::profiles::get_instance_mut(&context, &mut locks).await?
        else {
            return Ok(());
        };
        for st in instance.states.iter_mut() {
            st.text_placement = placement;
        }
        ui::emit(UiEvent::ActionStateChanged {
            context: instance.context.clone(),
        });
    }

    let apply_ctx = apply_context_for_visuals(&context);
    let (apply_ctx, apply_img, apply_overlays, apply_active) = {
        let Some(instance) =
            crate::store::profiles::get_instance_mut(&apply_ctx, &mut locks).await?
        else {
            return Ok(());
        };
        ui::emit(UiEvent::ActionStateChanged {
            context: instance.context.clone(),
        });
        let img = crate::events::outbound::devices::effective_image_for_instance(instance);
        (
            instance.context.clone(),
            img,
            crate::events::outbound::devices::overlays_for_instance(instance),
            active_profile && active_page == instance.context.page,
        )
    };
    save_profile(&context.device, &mut locks).await?;
    if apply_active {
        let _ = crate::events::outbound::devices::update_image_with_overlays(
            (&apply_ctx).into(),
            apply_img,
            apply_overlays,
        )
        .await;
    }
    Ok(())
}

pub async fn set_button_label_font_size(
    context: ActionContext,
    size: u16,
) -> Result<(), anyhow::Error> {
    let mut locks = acquire_locks_mut().await;
    let active_profile = locks
        .device_stores
        .get_selected_profile(&context.device)
        .ok()
        .is_some_and(|p| p == context.profile);
    let active_page = if active_profile {
        locks
            .profile_stores
            .get_profile_store_mut(&DEVICES.get(&context.device).unwrap(), &context.profile)
            .await?
            .value
            .selected_page
            .clone()
    } else {
        String::new()
    };
    {
        let Some(instance) = crate::store::profiles::get_instance_mut(&context, &mut locks).await?
        else {
            return Ok(());
        };
        for st in instance.states.iter_mut() {
            st.size = FontSize(size);
        }
        ui::emit(UiEvent::ActionStateChanged {
            context: instance.context.clone(),
        });
    }

    let apply_ctx = apply_context_for_visuals(&context);
    let (apply_ctx, apply_img, apply_overlays, apply_active) = {
        let Some(instance) =
            crate::store::profiles::get_instance_mut(&apply_ctx, &mut locks).await?
        else {
            return Ok(());
        };
        ui::emit(UiEvent::ActionStateChanged {
            context: instance.context.clone(),
        });
        let img = crate::events::outbound::devices::effective_image_for_instance(instance);
        (
            instance.context.clone(),
            img,
            crate::events::outbound::devices::overlays_for_instance(instance),
            active_profile && active_page == instance.context.page,
        )
    };
    save_profile(&context.device, &mut locks).await?;
    if apply_active {
        let _ = crate::events::outbound::devices::update_image_with_overlays(
            (&apply_ctx).into(),
            apply_img,
            apply_overlays,
        )
        .await;
    }
    Ok(())
}

pub async fn set_button_label_colour(
    context: ActionContext,
    colour: String,
) -> Result<(), anyhow::Error> {
    let mut locks = acquire_locks_mut().await;
    let active_profile = locks
        .device_stores
        .get_selected_profile(&context.device)
        .ok()
        .is_some_and(|p| p == context.profile);
    let active_page = if active_profile {
        locks
            .profile_stores
            .get_profile_store_mut(&DEVICES.get(&context.device).unwrap(), &context.profile)
            .await?
            .value
            .selected_page
            .clone()
    } else {
        String::new()
    };
    {
        let Some(instance) = crate::store::profiles::get_instance_mut(&context, &mut locks).await?
        else {
            return Ok(());
        };
        for st in instance.states.iter_mut() {
            st.colour = colour.clone();
        }
        ui::emit(UiEvent::ActionStateChanged {
            context: instance.context.clone(),
        });
    }

    let apply_ctx = apply_context_for_visuals(&context);
    let (apply_ctx, apply_img, apply_overlays, apply_active) = {
        let Some(instance) =
            crate::store::profiles::get_instance_mut(&apply_ctx, &mut locks).await?
        else {
            return Ok(());
        };
        ui::emit(UiEvent::ActionStateChanged {
            context: instance.context.clone(),
        });
        let img = crate::events::outbound::devices::effective_image_for_instance(instance);
        (
            instance.context.clone(),
            img,
            crate::events::outbound::devices::overlays_for_instance(instance),
            active_profile && active_page == instance.context.page,
        )
    };
    save_profile(&context.device, &mut locks).await?;
    if apply_active {
        let _ = crate::events::outbound::devices::update_image_with_overlays(
            (&apply_ctx).into(),
            apply_img,
            apply_overlays,
        )
        .await;
    }
    Ok(())
}

pub async fn set_button_show_title(
    context: ActionContext,
    show_title: bool,
) -> Result<(), anyhow::Error> {
    let mut locks = acquire_locks_mut().await;
    let active_profile = locks
        .device_stores
        .get_selected_profile(&context.device)
        .ok()
        .is_some_and(|p| p == context.profile);
    let active_page = if active_profile {
        locks
            .profile_stores
            .get_profile_store_mut(&DEVICES.get(&context.device).unwrap(), &context.profile)
            .await?
            .value
            .selected_page
            .clone()
    } else {
        String::new()
    };
    {
        let Some(instance) = crate::store::profiles::get_instance_mut(&context, &mut locks).await?
        else {
            return Ok(());
        };
        for st in instance.states.iter_mut() {
            st.show = show_title;
        }
        ui::emit(UiEvent::ActionStateChanged {
            context: instance.context.clone(),
        });
    }

    let apply_ctx = apply_context_for_visuals(&context);
    let (apply_ctx, apply_img, apply_overlays, apply_active) = {
        let Some(instance) =
            crate::store::profiles::get_instance_mut(&apply_ctx, &mut locks).await?
        else {
            return Ok(());
        };
        ui::emit(UiEvent::ActionStateChanged {
            context: instance.context.clone(),
        });
        let img = crate::events::outbound::devices::effective_image_for_instance(instance);
        (
            instance.context.clone(),
            img,
            crate::events::outbound::devices::overlays_for_instance(instance),
            active_profile && active_page == instance.context.page,
        )
    };
    save_profile(&context.device, &mut locks).await?;
    if apply_active {
        let _ = crate::events::outbound::devices::update_image_with_overlays(
            (&apply_ctx).into(),
            apply_img,
            apply_overlays,
        )
        .await;
    }
    Ok(())
}

pub async fn set_button_show_action_name(
    context: ActionContext,
    show_action_name: bool,
) -> Result<(), anyhow::Error> {
    let mut locks = acquire_locks_mut().await;
    let active_profile = locks
        .device_stores
        .get_selected_profile(&context.device)
        .ok()
        .is_some_and(|p| p == context.profile);
    let active_page = if active_profile {
        locks
            .profile_stores
            .get_profile_store_mut(&DEVICES.get(&context.device).unwrap(), &context.profile)
            .await?
            .value
            .selected_page
            .clone()
    } else {
        String::new()
    };
    {
        let Some(instance) = crate::store::profiles::get_instance_mut(&context, &mut locks).await?
        else {
            return Ok(());
        };
        for st in instance.states.iter_mut() {
            st.show_action_name = show_action_name;
        }
        ui::emit(UiEvent::ActionStateChanged {
            context: instance.context.clone(),
        });
    }

    let apply_ctx = apply_context_for_visuals(&context);
    let (apply_ctx, apply_img, apply_overlays, apply_active) = {
        let Some(instance) =
            crate::store::profiles::get_instance_mut(&apply_ctx, &mut locks).await?
        else {
            return Ok(());
        };
        ui::emit(UiEvent::ActionStateChanged {
            context: instance.context.clone(),
        });
        let img = crate::events::outbound::devices::effective_image_for_instance(instance);
        (
            instance.context.clone(),
            img,
            crate::events::outbound::devices::overlays_for_instance(instance),
            active_profile && active_page == instance.context.page,
        )
    };
    save_profile(&context.device, &mut locks).await?;
    if apply_active {
        let _ = crate::events::outbound::devices::update_image_with_overlays(
            (&apply_ctx).into(),
            apply_img,
            apply_overlays,
        )
        .await;
    }
    Ok(())
}

pub async fn set_button_show_icon(
    context: ActionContext,
    show_icon: bool,
) -> Result<(), anyhow::Error> {
    let mut locks = acquire_locks_mut().await;
    let active_profile = locks
        .device_stores
        .get_selected_profile(&context.device)
        .ok()
        .is_some_and(|p| p == context.profile);
    let active_page = if active_profile {
        locks
            .profile_stores
            .get_profile_store_mut(&DEVICES.get(&context.device).unwrap(), &context.profile)
            .await?
            .value
            .selected_page
            .clone()
    } else {
        String::new()
    };
    {
        let Some(instance) = crate::store::profiles::get_instance_mut(&context, &mut locks).await?
        else {
            return Ok(());
        };
        for st in instance.states.iter_mut() {
            st.show_icon = show_icon;
        }
        ui::emit(UiEvent::ActionStateChanged {
            context: instance.context.clone(),
        });
    }

    let apply_ctx = apply_context_for_visuals(&context);
    let (apply_ctx, apply_img, apply_overlays, apply_active) = {
        let Some(instance) =
            crate::store::profiles::get_instance_mut(&apply_ctx, &mut locks).await?
        else {
            return Ok(());
        };
        ui::emit(UiEvent::ActionStateChanged {
            context: instance.context.clone(),
        });
        let img = crate::events::outbound::devices::effective_image_for_instance(instance);
        (
            instance.context.clone(),
            img,
            crate::events::outbound::devices::overlays_for_instance(instance),
            active_profile && active_page == instance.context.page,
        )
    };
    save_profile(&context.device, &mut locks).await?;
    if apply_active {
        let _ = crate::events::outbound::devices::update_image_with_overlays(
            (&apply_ctx).into(),
            apply_img,
            apply_overlays,
        )
        .await;
    }
    Ok(())
}

fn instance_images_dir(context: &ActionContext) -> std::path::PathBuf {
    config_dir()
        .join("images")
        .join(&context.device)
        .join(&context.profile)
        .join(&context.page)
        .join(format!(
            "{}.{}.{}",
            context.controller, context.position, context.index
        ))
}

pub async fn set_custom_icon_from_path(
    context: ActionContext,
    state: Option<u16>,
    source_path: String,
) -> Result<(), anyhow::Error> {
    use std::path::Path;

    let src = Path::new(source_path.trim());
    if !src.is_file() {
        return Err(anyhow::anyhow!("icon path not found"));
    }
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("png")
        .to_lowercase();
    let ext = match ext.as_str() {
        "png" | "jpg" | "jpeg" | "webp" => ext,
        _ => {
            return Err(anyhow::anyhow!(
                "unsupported image type (use png/jpg/jpeg/webp)"
            ));
        }
    };

    let mut locks = acquire_locks_mut().await;
    let active = locks
        .device_stores
        .get_selected_profile(&context.device)
        .ok()
        .is_some_and(|p| p == context.profile);

    let (apply_ctx, apply_img, apply_overlays, apply_active) = {
        // Two-phase update so we can update parent visuals correctly for Multi/Toggle children.
        // Phase 1: write the custom icon into the target (child or parent) instance state.
        {
            let Some(instance) =
                crate::store::profiles::get_instance_mut(&context, &mut locks).await?
            else {
                return Ok(());
            };

            let dst_dir = instance_images_dir(&context);
            tokio::fs::create_dir_all(&dst_dir).await?;
            let dst = dst_dir.join(format!("custom_icon.{ext}"));
            tokio::fs::copy(src, &dst).await?;
            let dst_str = dst.to_string_lossy().into_owned();

            let target_state = state.unwrap_or(instance.current_state);
            if (target_state as usize) < instance.states.len() {
                instance.states[target_state as usize].image = dst_str.clone();
            }

            ui::emit(UiEvent::ActionStateChanged {
                context: context.clone(),
            });
        }

        // Phase 2: compute the correct button visual to apply.
        // If this was a child of a Multi/Toggle action, apply the parent (index 0) visual.
        let apply_ctx = if context.index != 0 {
            ActionContext::from_context((&context).into(), 0)
        } else {
            context.clone()
        };

        let Some(apply_instance) =
            crate::store::profiles::get_instance_mut(&apply_ctx, &mut locks).await?
        else {
            return Ok(());
        };

        if crate::shared::is_toggle_action_uuid(apply_instance.action.uuid.as_str()) {
            sync_toggle_state_images_from_children(apply_instance);
        }

        ui::emit(UiEvent::ActionStateChanged {
            context: apply_instance.context.clone(),
        });

        let apply_active = active && apply_instance.context.page == context.page;
        let img = crate::events::outbound::devices::effective_image_for_instance(apply_instance);
        let overlays = crate::events::outbound::devices::overlays_for_instance(apply_instance);
        (apply_instance.context.clone(), img, overlays, apply_active)
    };

    save_profile(&context.device, &mut locks).await?;

    if apply_active {
        let _ = crate::events::outbound::devices::update_image_with_overlays(
            (&apply_ctx).into(),
            apply_img,
            apply_overlays,
        )
        .await;
    }

    Ok(())
}

pub async fn clear_custom_icon(
    context: ActionContext,
    state: Option<u16>,
) -> Result<(), anyhow::Error> {
    let mut locks = acquire_locks_mut().await;
    let active = locks
        .device_stores
        .get_selected_profile(&context.device)
        .ok()
        .is_some_and(|p| p == context.profile);

    let (apply_ctx, apply_img, apply_overlays, apply_active) = {
        {
            let Some(instance) =
                crate::store::profiles::get_instance_mut(&context, &mut locks).await?
            else {
                return Ok(());
            };

            let target_state = state.unwrap_or(instance.current_state);
            if let (Some(s), Some(def)) = (
                instance.states.get_mut(target_state as usize),
                instance.action.states.get(target_state as usize),
            ) {
                s.image = def.image.clone();
            }

            ui::emit(UiEvent::ActionStateChanged {
                context: context.clone(),
            });
        }

        let apply_ctx = if context.index != 0 {
            ActionContext::from_context((&context).into(), 0)
        } else {
            context.clone()
        };

        let Some(apply_instance) =
            crate::store::profiles::get_instance_mut(&apply_ctx, &mut locks).await?
        else {
            return Ok(());
        };

        if crate::shared::is_toggle_action_uuid(apply_instance.action.uuid.as_str()) {
            sync_toggle_state_images_from_children(apply_instance);
        }

        ui::emit(UiEvent::ActionStateChanged {
            context: apply_instance.context.clone(),
        });

        let apply_active = active && apply_instance.context.page == context.page;
        let img = crate::events::outbound::devices::effective_image_for_instance(apply_instance);
        let overlays = crate::events::outbound::devices::overlays_for_instance(apply_instance);
        (apply_instance.context.clone(), img, overlays, apply_active)
    };

    save_profile(&context.device, &mut locks).await?;

    if apply_active {
        let _ = crate::events::outbound::devices::update_image_with_overlays(
            (&apply_ctx).into(),
            apply_img,
            apply_overlays,
        )
        .await;
    }

    Ok(())
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

/// Swap two action instances between slots.
///
/// Semantics:
/// - If both slots are empty: no-op.
/// - If exactly one slot is occupied: moves the instance to the other slot (source becomes empty).
/// - If both slots are occupied: swaps the two instances.
///
/// Constraints:
/// - Only supports swapping within the same controller type (Keypad↔Keypad or Encoder↔Encoder).
/// - Both contexts must refer to the same device/profile/page (UI expectation).
pub async fn swap_instances(a: Context, b: Context) -> Result<(), anyhow::Error> {
    if a == b {
        return Ok(());
    }
    if a.controller != b.controller {
        return Ok(());
    }
    if a.device != b.device || a.profile != b.profile || a.page != b.page {
        return Err(anyhow::anyhow!(
            "swap_instances requires same device/profile/page"
        ));
    }

    // Snapshot occupancy first (read-only lock).
    let (a_has, b_has) = {
        let locks = crate::store::profiles::acquire_locks().await;
        let a_slot = crate::store::profiles::get_slot(&a, &locks).await?;
        let b_slot = crate::store::profiles::get_slot(&b, &locks).await?;
        (a_slot.is_some(), b_slot.is_some())
    };

    match (a_has, b_has) {
        (false, false) => Ok(()),
        (true, false) => {
            let _ = move_instance(a, b, false).await?;
            Ok(())
        }
        (false, true) => {
            let _ = move_instance(b, a, false).await?;
            Ok(())
        }
        (true, true) => {
            let mut locks = acquire_locks_mut().await;

            // Take both instances out of their slots (avoid holding two mutable borrows at once).
            let mut a_inst = {
                let slot = get_slot_mut(&a, &mut locks).await?;
                slot.take()
            }
            .unwrap();
            let mut b_inst = {
                let slot = get_slot_mut(&b, &mut locks).await?;
                slot.take()
            }
            .unwrap();

            // Preserve old contexts for willDisappear notifications.
            let a_old_inst = a_inst.clone();
            let b_old_inst = b_inst.clone();

            let a_old_ctx = a_inst.context.clone();
            let b_old_ctx = b_inst.context.clone();

            // Retarget contexts (including children) to the new slot coordinates.
            let retarget = |instance: &mut ActionInstance, destination: &Context| {
                instance.context = ActionContext::from_context(destination.clone(), 0);
                if let Some(children) = &mut instance.children {
                    for (index, child) in children.iter_mut().enumerate() {
                        child.context =
                            ActionContext::from_context(destination.clone(), index as u16 + 1);
                        // Keep child state images valid (match existing move_instance behavior).
                        for (i, st) in child.states.iter_mut().enumerate() {
                            if !child.action.states[i].image.is_empty() {
                                st.image = child.action.states[i].image.clone();
                            } else {
                                st.image = child.action.icon.clone();
                            }
                        }
                    }
                }
            };
            retarget(&mut a_inst, &b);
            retarget(&mut b_inst, &a);

            // Swap custom icon directories (index 0) and rewrite any state image paths that point
            // inside those directories.
            let a_old_dir = instance_images_dir(&a_old_ctx);
            let b_old_dir = instance_images_dir(&b_old_ctx);
            let a_new_dir = instance_images_dir(&a_inst.context);
            let b_new_dir = instance_images_dir(&b_inst.context);

            // Best-effort directory swap. If a dir doesn't exist, treat it as empty.
            let safe = |s: &str| s.replace(['/', '\\'], "_");
            let tmp_dir = config_dir().join("images").join(format!(
                "__swap_tmp_{}_{}_{}_{}_{}_{}",
                safe(&a.device),
                safe(&a.profile),
                safe(&a.page),
                safe(&a.controller),
                a.position,
                b.position
            ));
            let _ = remove_dir_all(&tmp_dir).await;
            let _ = tokio::fs::create_dir_all(tmp_dir.parent().unwrap()).await;
            if a_old_dir.exists() {
                let _ = tokio::fs::rename(&a_old_dir, &tmp_dir).await;
            }
            if b_old_dir.exists() {
                let _ = tokio::fs::create_dir_all(a_old_dir.parent().unwrap()).await;
                let _ = tokio::fs::rename(&b_old_dir, &a_old_dir).await;
            }
            if tmp_dir.exists() {
                let _ = tokio::fs::create_dir_all(b_old_dir.parent().unwrap()).await;
                let _ = tokio::fs::rename(&tmp_dir, &b_old_dir).await;
            }

            for st in a_inst.states.iter_mut() {
                let path = std::path::Path::new(&st.image);
                if path.starts_with(&a_old_dir) {
                    st.image = a_new_dir
                        .join(path.strip_prefix(&a_old_dir).unwrap())
                        .to_string_lossy()
                        .into_owned();
                }
            }
            for st in b_inst.states.iter_mut() {
                let path = std::path::Path::new(&st.image);
                if path.starts_with(&b_old_dir) {
                    st.image = b_new_dir
                        .join(path.strip_prefix(&b_old_dir).unwrap())
                        .to_string_lossy()
                        .into_owned();
                }
            }

            // Notify plugins that the instances disappeared from their old contexts.
            // We ignore errors here as in other APIs (best-effort).
            let _ = crate::events::outbound::will_appear::will_disappear(&a_old_inst, true).await;
            let _ = crate::events::outbound::will_appear::will_disappear(&b_old_inst, true).await;

            // Put swapped instances back into their new slots.
            {
                let slot = get_slot_mut(&a, &mut locks).await?;
                *slot = Some(b_inst.clone());
            }
            {
                let slot = get_slot_mut(&b, &mut locks).await?;
                *slot = Some(a_inst.clone());
            }

            // Notify plugins that the instances appeared at their new contexts.
            let _ = crate::events::outbound::will_appear::will_appear(&b_inst).await;
            let _ = crate::events::outbound::will_appear::will_appear(&a_inst).await;

            save_profile(&a.device, &mut locks).await?;
            ui::emit(UiEvent::ActionStateChanged {
                context: a_inst.context.clone(),
            });
            ui::emit(UiEvent::ActionStateChanged {
                context: b_inst.context.clone(),
            });

            Ok(())
        }
    }
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
        if crate::shared::is_toggle_action_uuid(instance.action.uuid.as_str()) {
            if instance.current_state as usize >= children.len() {
                instance.current_state = if children.is_empty() {
                    0
                } else {
                    children.len() as u16 - 1
                };
            }
            if !children.is_empty() {
                instance.states.pop();
            }
            sync_toggle_state_images_from_children(instance);
            ui::emit(UiEvent::ActionStateChanged {
                context: instance.context.clone(),
            });
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

    let overlays = crate::events::outbound::devices::overlays_for_context(&context).await;
    if let Err(error) =
        crate::events::outbound::devices::update_image_with_overlays(context, Some(image), overlays)
            .await
    {
        log::warn!("Failed to update device image: {}", error);
    }
}
