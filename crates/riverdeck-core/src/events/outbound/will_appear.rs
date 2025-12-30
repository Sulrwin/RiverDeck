use super::{GenericInstancePayload, send_to_plugin};

use crate::shared::{ActionContext, ActionInstance};

#[derive(serde::Serialize)]
struct AppearEvent {
    event: &'static str,
    action: String,
    context: ActionContext,
    device: String,
    payload: GenericInstancePayload,
}

pub async fn will_appear(instance: &ActionInstance) -> Result<(), anyhow::Error> {
    // Some instances are host-side/built-in (no plugin process), and some plugins may not
    // be connected yet. We still want the device to show something immediately.
    let plugin = instance.action.plugin.trim();
    if !plugin.is_empty() {
        // Best-effort: don't fail `will_appear` if the plugin isn't reachable; still proceed
        // with pushing initial icon/title to the device.
        let _ = send_to_plugin(
            plugin,
            &AppearEvent {
                event: "willAppear",
                action: instance.action.uuid.clone(),
                context: instance.context.clone(),
                device: instance.context.device.clone(),
                payload: GenericInstancePayload::new(instance),
            },
        )
        .await;
    }

    // Also best-effort: if this fails due to plugin comms, don't block device rendering.
    let _ = super::states::title_parameters_did_change(instance, instance.current_state).await;

    // Ensure something visible is pushed to the device immediately.
    // Many plugins rely on the host to show the manifest icon until they call `setImage`.
    let img = crate::events::outbound::devices::effective_image_for_instance(instance);
    if let Err(error) =
        crate::events::outbound::devices::update_image_for_instance(instance, img).await
    {
        log::warn!(
            "Failed to set initial device image on willAppear: {}",
            error
        );
    }

    Ok(())
}

pub async fn will_disappear(
    instance: &ActionInstance,
    clear_on_device: bool,
) -> Result<(), anyhow::Error> {
    let plugin = instance.action.plugin.trim();
    if !plugin.is_empty() {
        let _ = send_to_plugin(
            plugin,
            &AppearEvent {
                event: "willDisappear",
                action: instance.action.uuid.clone(),
                context: instance.context.clone(),
                device: instance.context.device.clone(),
                payload: GenericInstancePayload::new(instance),
            },
        )
        .await;
    }

    if clear_on_device
        && let Err(error) =
            crate::events::outbound::devices::update_image_for_instance(instance, None).await
    {
        log::warn!("Failed to clear device image: {}", error);
    }

    Ok(())
}
