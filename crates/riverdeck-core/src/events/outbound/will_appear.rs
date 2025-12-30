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
    send_to_plugin(
        &instance.action.plugin,
        &AppearEvent {
            event: "willAppear",
            action: instance.action.uuid.clone(),
            context: instance.context.clone(),
            device: instance.context.device.clone(),
            payload: GenericInstancePayload::new(instance),
        },
    )
    .await?;

    super::states::title_parameters_did_change(instance, instance.current_state).await?;

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
    send_to_plugin(
        &instance.action.plugin,
        &AppearEvent {
            event: "willDisappear",
            action: instance.action.uuid.clone(),
            context: instance.context.clone(),
            device: instance.context.device.clone(),
            payload: GenericInstancePayload::new(instance),
        },
    )
    .await?;

    if clear_on_device
        && let Err(error) =
            crate::events::outbound::devices::update_image_for_instance(instance, None).await
    {
        log::warn!("Failed to clear device image: {}", error);
    }

    Ok(())
}
