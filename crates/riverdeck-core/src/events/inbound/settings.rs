use crate::events::outbound::settings as outbound;
use crate::shared::ActionContext;
use crate::store::profiles::{
    acquire_locks, acquire_locks_mut, get_instance, get_instance_mut, save_profile,
};
use crate::ui::{self, UiEvent};

use std::str::FromStr;

fn is_starterpack_device_brightness_uuid(uuid: &str) -> bool {
    matches!(
        uuid,
        "io.github.sulrwin.riverdeck.starterpack.devicebrightness"
            | "com.amansprojects.starterpack.devicebrightness"
    )
}

fn normalize_device_brightness_action(s: &str) -> Option<&'static str> {
    match s.trim() {
        "" | "none" => None,
        "set" => Some("set"),
        "increase" => Some("increase"),
        "decrease" => Some("decrease"),
        _ => None,
    }
}

fn device_brightness_title_for_settings(settings: &serde_json::Value) -> String {
    let obj = settings.as_object();

    let get_str = |key: &str| -> Option<&str> {
        obj.and_then(|o| o.get(key))
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
    };
    let get_u8 = |key: &str| -> Option<u8> {
        obj.and_then(|o| o.get(key))
            .and_then(|v| v.as_u64())
            .map(|v| v as u8)
    };

    // Prefer schema keys; fall back to legacy PI keys.
    let press_action = get_str("pressAction")
        .and_then(normalize_device_brightness_action)
        .or_else(|| get_str("action").and_then(normalize_device_brightness_action))
        .unwrap_or("set");

    let set_value = get_u8("setValue")
        .or_else(|| get_u8("value"))
        .unwrap_or(50)
        .clamp(0, 100);

    // For increase/decrease actions, show the configured step/value as a percentage too.
    let step_or_value = get_u8("step")
        .or_else(|| get_u8("value"))
        .unwrap_or(1)
        .clamp(0, 100);

    let display = match press_action {
        "set" => set_value,
        "increase" | "decrease" => step_or_value,
        _ => set_value,
    };

    format!("{display}%")
}

pub async fn set_settings(
    event: super::ContextAndPayloadEvent<serde_json::Value>,
    from_property_inspector: bool,
) -> Result<(), anyhow::Error> {
    let mut locks = acquire_locks_mut().await;

    if let Some(instance) = get_instance_mut(&event.context, &mut locks).await? {
        instance.settings = event.payload;

        // Host-side convenience: keep Starterpack Device Brightness title as `NN%` so it behaves
        // like "CPU"/vitals-style actions that show live/config values on the key.
        if is_starterpack_device_brightness_uuid(instance.action.uuid.as_str()) {
            let title = device_brightness_title_for_settings(&instance.settings);
            for st in instance.states.iter_mut() {
                st.text = title.clone();
                st.show = true;
            }
            // Notify UI caches and repaint hardware (setSettings doesn't implicitly repaint).
            ui::emit(UiEvent::ActionStateChanged {
                context: instance.context.clone(),
            });
            let img = crate::events::outbound::devices::effective_image_for_instance(instance);
            let _ =
                crate::events::outbound::devices::update_image_for_instance(instance, img).await;
        }

        outbound::did_receive_settings(instance, !from_property_inspector).await?;
        save_profile(&event.context.device, &mut locks).await?;
    }

    Ok(())
}

pub async fn get_settings(
    event: super::ContextEvent,
    from_property_inspector: bool,
) -> Result<(), anyhow::Error> {
    let locks = acquire_locks().await;

    if let Some(instance) = get_instance(&event.context, &locks).await? {
        outbound::did_receive_settings(instance, from_property_inspector).await?;
    }

    Ok(())
}

pub async fn set_global_settings(
    event: super::ContextAndPayloadEvent<serde_json::Value, String>,
    from_property_inspector: bool,
) -> Result<(), anyhow::Error> {
    let uuid = if from_property_inspector {
        if let Some(instance) = get_instance(
            &ActionContext::from_str(&event.context)?,
            &acquire_locks().await,
        )
        .await?
        {
            instance.action.plugin.clone()
        } else {
            return Ok(());
        }
    } else {
        event.context.clone()
    };

    {
        let settings_dir = crate::shared::config_dir().join("settings");
        tokio::fs::create_dir_all(&settings_dir).await?;

        let path = settings_dir.join(uuid.clone() + ".json");
        tokio::fs::write(path, event.payload.to_string()).await?;
    }

    outbound::did_receive_global_settings(&uuid, !from_property_inspector).await?;

    Ok(())
}

pub async fn get_global_settings(
    event: super::ContextEvent<String>,
    from_property_inspector: bool,
) -> Result<(), anyhow::Error> {
    let uuid = if from_property_inspector {
        if let Some(instance) = get_instance(
            &ActionContext::from_str(&event.context)?,
            &acquire_locks().await,
        )
        .await?
        {
            instance.action.plugin.clone()
        } else {
            return Ok(());
        }
    } else {
        event.context.clone()
    };

    outbound::did_receive_global_settings(&uuid, from_property_inspector).await
}
