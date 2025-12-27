pub async fn get_settings() -> Result<crate::store::Settings, anyhow::Error> {
    Ok(crate::store::get_settings()?.value)
}

pub async fn set_settings(settings: crate::store::Settings) -> Result<(), anyhow::Error> {
    crate::events::outbound::devices::set_brightness(settings.brightness).await?;
    let mut store = crate::store::get_settings()?;
    store.value = settings;
    store.save()?;
    Ok(())
}

pub fn open_config_directory() -> Result<(), anyhow::Error> {
    open::that_detached(crate::shared::config_dir())?;
    Ok(())
}

pub fn open_log_directory() -> Result<(), anyhow::Error> {
    open::that_detached(crate::shared::log_dir())?;
    Ok(())
}

pub fn get_build_info() -> String {
    // Core does not embed the detailed Tauri built_info; provide a minimal string for now.
    format!(
        "{} v{} ({})",
        crate::shared::PRODUCT_NAME,
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS
    )
}
