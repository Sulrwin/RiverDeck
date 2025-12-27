use super::{ContextEvent, PayloadEvent};

use serde::{Deserialize, Serialize};

use crate::ui::{self, UiEvent};

#[derive(Deserialize)]
pub struct OpenUrlEvent {
    pub url: String,
}

pub async fn open_url(event: PayloadEvent<OpenUrlEvent>) -> Result<(), anyhow::Error> {
    // Avoid turning RiverDeck into a general-purpose local "open arbitrary thing" endpoint.
    // Most Stream Deck plugins use this for http(s) links.
    let url = event.payload.url.trim();
    if url.len() > 2048 {
        return Ok(());
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Ok(());
    }
    log::debug!("Opening URL {}", url);
    open::that_detached(url)?;
    Ok(())
}

#[derive(Deserialize)]
pub struct LogMessageEvent {
    pub message: String,
}

pub async fn log_message(
    uuid: Option<&str>,
    mut event: PayloadEvent<LogMessageEvent>,
) -> Result<(), anyhow::Error> {
    if let Some(uuid) = uuid
        && let Ok(manifest) = crate::plugins::manifest::read_manifest(
            &crate::shared::config_dir().join("plugins").join(uuid),
        )
    {
        event.payload.message = format!("[{}] {}", manifest.name, event.payload.message);
    }
    log::info!("{}", event.payload.message.trim());
    Ok(())
}

pub async fn show_alert(event: ContextEvent) -> Result<(), anyhow::Error> {
    ui::emit(UiEvent::ShowAlert {
        context: event.context.into(),
    });
    Ok(())
}

pub async fn show_ok(event: ContextEvent) -> Result<(), anyhow::Error> {
    ui::emit(UiEvent::ShowOk {
        context: event.context.into(),
    });
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SwitchProfileEvent {
    device: String,
    profile: String,
}

pub async fn switch_profile(event: SwitchProfileEvent) -> Result<(), anyhow::Error> {
    ui::emit(UiEvent::SwitchProfile {
        device: event.device,
        profile: event.profile,
    });
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
pub struct DeviceBrightnessEvent {
    action: String,
    value: u8,
}

pub async fn device_brightness(event: DeviceBrightnessEvent) -> Result<(), anyhow::Error> {
    ui::emit(UiEvent::DeviceBrightness {
        action: event.action,
        value: event.value,
    });
    Ok(())
}
