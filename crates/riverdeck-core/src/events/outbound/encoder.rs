use super::{Coordinates, send_to_plugin};

use crate::shared::{ActionContext, DEVICES};
use crate::store::profiles::{acquire_locks_mut, get_instance_mut};
use crate::ui::{self, UiEvent};

use once_cell::sync::Lazy;
use serde::Serialize;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::Mutex;

struct PendingDialRotate {
    plugin: String,
    action_uuid: String,
    ctx: ActionContext,
    settings: serde_json::Value,
    coords: Coordinates,
    ticks: i16,
}

static PENDING_DIAL_ROTATE: Lazy<Mutex<HashMap<String, PendingDialRotate>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

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
    // IMPORTANT: do not hold profile locks while awaiting network IO to the plugin.
    // Dial rotations can be high-frequency; backpressure should not stall the entire input pipeline.
    let (plugin, action_uuid, ctx, settings, coords) = {
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
        (
            instance.action.plugin.clone(),
            instance.action.uuid.clone(),
            instance.context.clone(),
            instance.settings.clone(),
            Coordinates {
                row: instance.context.position / 3,
                column: instance.context.position % 3,
            },
        )
    };

    // If there's no plugin process (host-side/built-in), nothing to send.
    if plugin.trim().is_empty() {
        return Ok(());
    }

    // Coalesce rapid dial ticks into a short (~16ms) batch per encoder context.
    // This keeps fast spins responsive even when the plugin/UI can't process hundreds of
    // websocket messages per second.
    let key = ctx.to_string();
    let should_spawn = {
        let mut map = PENDING_DIAL_ROTATE.lock().await;
        if let Some(p) = map.get_mut(&key) {
            p.ticks = p.ticks.saturating_add(ticks);
            // Keep most recent metadata (settings can change).
            p.plugin = plugin;
            p.action_uuid = action_uuid;
            p.ctx = ctx;
            p.settings = settings;
            p.coords = coords;
            false
        } else {
            map.insert(
                key.clone(),
                PendingDialRotate {
                    plugin,
                    action_uuid,
                    ctx,
                    settings,
                    coords,
                    ticks,
                },
            );
            true
        }
    };

    if should_spawn {
        tokio::spawn(async move {
            // One frame-ish delay to gather bursts.
            tokio::time::sleep(Duration::from_millis(16)).await;
            let pending = { PENDING_DIAL_ROTATE.lock().await.remove(&key) };
            let Some(p) = pending else { return };
            if p.ticks == 0 {
                return;
            }
            let _ = send_to_plugin(
                &p.plugin,
                &DialRotateEvent {
                    event: "dialRotate",
                    action: p.action_uuid,
                    context: p.ctx.clone(),
                    device: p.ctx.device.clone(),
                    payload: DialRotatePayload {
                        settings: p.settings,
                        coordinates: p.coords,
                        ticks: p.ticks,
                        pressed: false,
                    },
                },
            )
            .await;
        });
    }

    Ok(())
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
    // IMPORTANT: do not hold profile locks while awaiting network IO to the plugin.
    let (plugin, action_uuid, ctx, settings, coords) = {
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
        (
            instance.action.plugin.clone(),
            instance.action.uuid.clone(),
            instance.context.clone(),
            instance.settings.clone(),
            Coordinates {
                row: instance.context.position / 3,
                column: instance.context.position % 3,
            },
        )
    };

    ui::emit(UiEvent::KeyMoved {
        context: ctx.clone().into(),
        down: event == "dialDown",
    });

    send_to_plugin(
        &plugin,
        &DialPressEvent {
            event,
            action: action_uuid,
            context: ctx.clone(),
            device: ctx.device.clone(),
            payload: DialPressPayload {
                controller: "Encoder",
                settings,
                coordinates: coords,
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

#[derive(Serialize)]
#[allow(non_snake_case)]
struct TouchSwipePayload {
    controller: &'static str,
    settings: serde_json::Value,
    coordinates: Coordinates,
    resources: HashMap<String, String>,
    swipeFrom: [u16; 2],
    swipeTo: [u16; 2],
}

#[derive(Serialize)]
struct TouchSwipeEvent {
    event: &'static str,
    action: String,
    context: ActionContext,
    device: String,
    payload: TouchSwipePayload,
}

pub async fn touch_swipe(
    device: &str,
    start: (u16, u16),
    end: (u16, u16),
) -> Result<(), anyhow::Error> {
    let encoders = DEVICES.get(device).map(|d| d.encoders).unwrap_or(0);
    if encoders == 0 {
        return Ok(());
    }

    // Stream Deck+ LCD is 800px wide with 4 segments of 200px each. Route the swipe to the
    // encoder whose segment contains the start x coordinate.
    let idx = ((start.0 as u32) / 200).min(encoders.saturating_sub(1) as u32) as u8;

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
        &TouchSwipeEvent {
            event: "touchSwipe",
            action: instance.action.uuid.clone(),
            context: instance.context.clone(),
            device: instance.context.device.clone(),
            payload: TouchSwipePayload {
                controller: "Encoder",
                settings: instance.settings.clone(),
                coordinates: Coordinates {
                    row: instance.context.position / 3,
                    column: instance.context.position % 3,
                },
                resources: HashMap::new(),
                swipeFrom: [start.0, start.1],
                swipeTo: [end.0, end.1],
            },
        },
    )
    .await
}
