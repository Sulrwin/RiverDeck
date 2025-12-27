use super::{send_to_all_plugins, send_to_plugin};

use crate::plugins::{DEVICE_NAMESPACES, info_param::DeviceInfo};
use crate::shared::{ActionInstance, TextPlacement};
use crate::store::profiles;

use serde::Serialize;

#[derive(Serialize)]
#[allow(non_snake_case)]
struct DeviceDidConnectEvent {
    event: &'static str,
    device: String,
    deviceInfo: DeviceInfo,
}

pub async fn device_did_connect(id: &str, info: DeviceInfo) -> Result<(), anyhow::Error> {
    send_to_all_plugins(&DeviceDidConnectEvent {
        event: "deviceDidConnect",
        device: id.to_owned(),
        deviceInfo: info,
    })
    .await
}

#[derive(Serialize)]
struct DeviceDidDisconnectEvent {
    event: &'static str,
    device: String,
}

pub async fn device_did_disconnect(id: &str) -> Result<(), anyhow::Error> {
    send_to_all_plugins(&DeviceDidDisconnectEvent {
        event: "deviceDidDisconnect",
        device: id.to_owned(),
    })
    .await
}

#[derive(Serialize)]
struct SetImageEvent {
    event: &'static str,
    device: String,
    controller: Option<String>,
    position: Option<u8>,
    image: Option<String>,
}

pub async fn update_image(
    context: crate::shared::Context,
    image: Option<String>,
) -> Result<(), anyhow::Error> {
    update_image_with_overlays(context, image, None).await
}

pub(crate) fn overlays_for_instance(
    instance: &ActionInstance,
) -> Option<Vec<(String, TextPlacement)>> {
    let st = instance.states.get(instance.current_state as usize)?;

    let title = st.text.trim();
    let action_name = instance.action.name.trim();
    let show_title = st.show && !title.is_empty();
    let show_action_name = st.show_action_name && !action_name.is_empty();

    if !show_title && !show_action_name {
        return None;
    }

    let mut overlays: Vec<(String, TextPlacement)> = Vec::new();

    // Keep legacy behavior: the Stream Deck "Title" uses `text_placement`.
    if show_title {
        overlays.push((title.to_owned(), st.text_placement));
    }

    // If the title already equals the action name (common default), don't render both.
    if show_action_name && (!show_title || title != action_name) {
        let opposite = |p: TextPlacement| match p {
            TextPlacement::Top => TextPlacement::Bottom,
            TextPlacement::Bottom => TextPlacement::Top,
            TextPlacement::Left => TextPlacement::Right,
            TextPlacement::Right => TextPlacement::Left,
        };
        let placement = if show_title {
            opposite(st.text_placement)
        } else {
            // If there's no title, keep the action name in the familiar place.
            TextPlacement::Bottom
        };
        overlays.push((action_name.to_owned(), placement));
    }

    Some(overlays)
}

pub async fn update_image_for_instance(
    instance: &ActionInstance,
    image: Option<String>,
) -> Result<(), anyhow::Error> {
    update_image_with_overlays(
        (&instance.context).into(),
        image,
        overlays_for_instance(instance),
    )
    .await
}

pub async fn overlays_for_context(
    context: &crate::shared::Context,
) -> Option<Vec<(String, TextPlacement)>> {
    // This is an async lookup that can await locks safely. Do NOT call this from places that
    // already hold profile locks; prefer `overlays_for_instance()` in those cases.
    let locks = profiles::acquire_locks().await;
    let slot = profiles::get_slot(context, &locks).await.ok()?;
    overlays_for_instance(slot.as_ref()?)
}

pub async fn update_image_with_overlays(
    context: crate::shared::Context,
    image: Option<String>,
    overlays: Option<Vec<(String, TextPlacement)>>,
) -> Result<(), anyhow::Error> {
    if let Some(plugin) = DEVICE_NAMESPACES.read().await.get(&context.device[..2]) {
        send_to_plugin(
            plugin,
            &SetImageEvent {
                event: "setImage",
                device: context.device,
                controller: Some(context.controller),
                position: Some(context.position),
                image,
            },
        )
        .await?;
    } else if context.device.starts_with("sd-") {
        crate::elgato::update_image(&context, image.as_deref(), overlays).await?;
    }

    Ok(())
}

pub async fn clear_screen(device: String) -> Result<(), anyhow::Error> {
    if let Some(plugin) = DEVICE_NAMESPACES.read().await.get(&device[..2]) {
        send_to_plugin(
            plugin,
            &SetImageEvent {
                event: "setImage",
                device,
                controller: None,
                position: None,
                image: None,
            },
        )
        .await?;
    } else if device.starts_with("sd-") {
        crate::elgato::clear_screen(&device).await?;
    }

    Ok(())
}

#[derive(Serialize)]
struct SetBrightnessEvent {
    event: &'static str,
    device: String,
    brightness: u8,
}

pub async fn set_brightness(brightness: u8) -> Result<(), anyhow::Error> {
    let namespaces = DEVICE_NAMESPACES.read().await;
    for device in crate::shared::DEVICES.iter() {
        if let Some(plugin) = namespaces.get(&device.id[..2]) {
            send_to_plugin(
                plugin,
                &SetBrightnessEvent {
                    event: "setBrightness",
                    device: device.id.clone(),
                    brightness,
                },
            )
            .await?;
        }
    }
    crate::elgato::set_brightness(brightness).await;

    Ok(())
}
