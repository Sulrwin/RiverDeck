use crate::shared::ActionContext;

pub async fn make_info(plugin: String) -> Result<crate::plugins::info_param::Info, anyhow::Error> {
    let manifest = crate::plugins::manifest::read_manifest(
        &crate::shared::config_dir().join("plugins").join(&plugin),
    )?;
    Ok(crate::plugins::info_param::make_info(plugin, manifest.version, false).await)
}

pub async fn switch_property_inspector(old: Option<ActionContext>, new: Option<ActionContext>) {
    if let Some(context) = old {
        let _ = crate::events::outbound::property_inspector::property_inspector_did_appear(
            context,
            "propertyInspectorDidDisappear",
        )
        .await;
    }
    if let Some(context) = new {
        let _ = crate::events::outbound::property_inspector::property_inspector_did_appear(
            context,
            "propertyInspectorDidAppear",
        )
        .await;
    }
}

pub fn open_url(url: String) -> Result<(), anyhow::Error> {
    open::that_detached(url)?;
    Ok(())
}
