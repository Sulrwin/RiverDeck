use crate::shared::{ActionContext, Context};

use once_cell::sync::OnceCell;
use tokio::sync::broadcast;

#[derive(Debug, Clone)]
pub enum UiEvent {
    DevicesUpdated,
    ApplicationsUpdated {
        applications: Vec<String>,
    },
    SwitchProfile {
        device: String,
        profile: String,
    },
    KeyMoved {
        context: Context,
        down: bool,
    },
    ActionStateChanged {
        context: ActionContext,
    },
    ShowAlert {
        context: Context,
    },
    ShowOk {
        context: Context,
    },
    DeviceBrightness {
        action: String,
        value: u8,
    },
    /// A hint that the UI should rerender/reload images for a given device (e.g. after device connect).
    RerenderImages {
        device: String,
    },
}

static UI_EVENTS: OnceCell<broadcast::Sender<UiEvent>> = OnceCell::new();

pub fn init(sender: broadcast::Sender<UiEvent>) {
    let _ = UI_EVENTS.set(sender);
}

pub fn subscribe() -> Option<broadcast::Receiver<UiEvent>> {
    UI_EVENTS.get().map(|s| s.subscribe())
}

pub fn emit(event: UiEvent) {
    if let Some(sender) = UI_EVENTS.get() {
        let _ = sender.send(event);
    }
}
