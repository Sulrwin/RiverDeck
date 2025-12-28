use super::{Coordinates, send_to_plugin};

use crate::shared::{ActionContext, DEVICES};
use crate::store::profiles::{acquire_locks_mut, get_instance_mut};
use crate::ui::{self, UiEvent};

use serde::Serialize;
use std::collections::HashMap;

#[derive(Serialize)]
struct DialRotatePayload {
    settings: serde_json::Value,
    coordinates: Coordinates,
    ticks: i16,
    pressed: bool,
}

#[derive(Serialize)]
struct DialRotateEvent {
    event: &'static str,
    action: String,
    context: ActionContext,
    device: String,
    payload: DialRotatePayload,
}

pub async fn dial_rotate(device: &str, index: u8, ticks: i16) -> Result<(), anyhow::Error> {
    let mut locks = acquire_locks_mut().await;
    let selected_profile = locks.device_stores.get_selected_profile(device)?;
    let page = locks
        .profile_stores
        .get_profile_store_mut(&DEVICES.get(device).unwrap(), &selected_profile)
        .await?
        .value
        .selected_page
        .clone();
    let context = ActionContext {
        device: device.to_owned(),
        profile: selected_profile.to_owned(),
        page,
        controller: "Encoder".to_owned(),
        position: index,
        index: 0,
    };
    let Some(instance) = get_instance_mut(&context, &mut locks).await? else {
        return Ok(());
    };

    send_to_plugin(
        &instance.action.plugin,
        &DialRotateEvent {
            event: "dialRotate",
            action: instance.action.uuid.clone(),
            context: instance.context.clone(),
            device: instance.context.device.clone(),
            payload: DialRotatePayload {
                settings: instance.settings.clone(),
                coordinates: Coordinates {
                    row: instance.context.position / 3,
                    column: instance.context.position % 3,
                },
                ticks,
                pressed: false,
            },
        },
    )
    .await
}

#[derive(Serialize)]
struct DialPressPayload {
    controller: &'static str,
    settings: serde_json::Value,
    coordinates: Coordinates,
}

#[derive(Serialize)]
struct DialPressEvent {
    event: &'static str,
    action: String,
    context: ActionContext,
    device: String,
    payload: DialPressPayload,
}

pub async fn dial_press(device: &str, event: &'static str, index: u8) -> Result<(), anyhow::Error> {
    let mut locks = acquire_locks_mut().await;
    let selected_profile = locks.device_stores.get_selected_profile(device)?;
    let page = locks
        .profile_stores
        .get_profile_store_mut(&DEVICES.get(device).unwrap(), &selected_profile)
        .await?
        .value
        .selected_page
        .clone();
    let context = ActionContext {
        device: device.to_owned(),
        profile: selected_profile.to_owned(),
        page,
        controller: "Encoder".to_owned(),
        position: index,
        index: 0,
    };
    let Some(instance) = get_instance_mut(&context, &mut locks).await? else {
        return Ok(());
    };
    ui::emit(UiEvent::KeyMoved {
        context: context.clone().into(),
        down: event == "dialDown",
    });

    send_to_plugin(
        &instance.action.plugin,
        &DialPressEvent {
            event,
            action: instance.action.uuid.clone(),
            context: instance.context.clone(),
            device: instance.context.device.clone(),
            payload: DialPressPayload {
                controller: "Encoder",
                settings: instance.settings.clone(),
                coordinates: Coordinates {
                    row: instance.context.position / 3,
                    column: instance.context.position % 3,
                },
            },
        },
    )
    .await
}

#[derive(Serialize)]
#[allow(non_snake_case)]
struct TouchTapPayload {
    controller: &'static str,
    settings: serde_json::Value,
    coordinates: Coordinates,
    hold: bool,
    resources: HashMap<String, String>,
    tapPos: [u16; 2],
}

#[derive(Serialize)]
struct TouchTapEvent {
    event: &'static str,
    action: String,
    context: ActionContext,
    device: String,
    payload: TouchTapPayload,
}

pub async fn touch_tap(device: &str, x: u16, y: u16, hold: bool) -> Result<(), anyhow::Error> {
    let encoders = DEVICES.get(device).map(|d| d.encoders).unwrap_or(0);
    if encoders == 0 {
        return Ok(());
    }
    // Stream Deck+ LCD is 800px wide with 4 segments of 200px each. Route the touch to the
    // encoder whose segment contains the x coordinate.
    let idx = ((x as u32) / 200).min(encoders.saturating_sub(1) as u32) as u8;

    let mut locks = acquire_locks_mut().await;
    let selected_profile = locks.device_stores.get_selected_profile(device)?;
    let page = locks
        .profile_stores
        .get_profile_store_mut(&DEVICES.get(device).unwrap(), &selected_profile)
        .await?
        .value
        .selected_page
        .clone();
    let context = ActionContext {
        device: device.to_owned(),
        profile: selected_profile.to_owned(),
        page,
        controller: "Encoder".to_owned(),
        position: idx,
        index: 0,
    };
    let Some(instance) = get_instance_mut(&context, &mut locks).await? else {
        return Ok(());
    };

    send_to_plugin(
        &instance.action.plugin,
        &TouchTapEvent {
            event: "touchTap",
            action: instance.action.uuid.clone(),
            context: instance.context.clone(),
            device: instance.context.device.clone(),
            payload: TouchTapPayload {
                controller: "Encoder",
                settings: instance.settings.clone(),
                coordinates: Coordinates {
                    row: instance.context.position / 3,
                    column: instance.context.position % 3,
                },
                hold,
                resources: HashMap::new(),
                tapPos: [x, y],
            },
        },
    )
    .await
}
