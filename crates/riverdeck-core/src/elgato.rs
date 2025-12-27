use crate::events::outbound::{encoder, keypad};

use std::collections::HashMap;
use std::path::Path;

use base64::Engine as _;
use elgato_streamdeck::{
    AsyncStreamDeck, DeviceStateUpdate,
    images::{ImageRect, convert_image_with_format_async},
    info::Kind,
};
use once_cell::sync::Lazy;
use tokio::sync::RwLock;

static ELGATO_DEVICES: Lazy<RwLock<HashMap<String, AsyncStreamDeck>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

async fn load_dynamic_image(image: &str) -> Result<image::DynamicImage, anyhow::Error> {
    if image.trim().starts_with("data:") {
        // Stream Deck SDK commonly uses data URLs.
        // Support both base64 and "raw" (non-base64) payloads.
        let bytes = if image.contains(";base64,") {
            let (_meta, b64) = image
                .split_once(";base64,")
                .ok_or_else(|| anyhow::anyhow!("invalid data url (missing ';base64,')"))?;
            base64::engine::general_purpose::STANDARD.decode(b64)?
        } else {
            let (_meta, raw) = image
                .split_once(',')
                .ok_or_else(|| anyhow::anyhow!("invalid data url (missing ',')"))?;
            raw.as_bytes().to_vec()
        };
        Ok(image::load_from_memory(&bytes)?)
    } else {
        // RiverDeck stores images as filesystem paths in profiles (including decoded `data:` images).
        // Also allow built-in relative paths like `riverdeck/...` by resolving against config/resource dirs.
        let mut candidate: Option<std::path::PathBuf> = None;
        let p = Path::new(image.trim());
        if p.is_file() {
            candidate = Some(p.to_path_buf());
        } else if image.starts_with("riverdeck/") || image.starts_with("opendeck/") {
            let cfg = crate::shared::config_dir().join(image.trim());
            if cfg.is_file() {
                candidate = Some(cfg);
            } else if let Some(res) = crate::shared::resource_dir() {
                let rp = res.join(image.trim());
                if rp.is_file() {
                    candidate = Some(rp);
                }
            }
        }

        let path = candidate.ok_or_else(|| anyhow::anyhow!("image path not found: {image}"))?;
        Ok(image::open(path)?)
    }
}

pub async fn update_image(
    context: &crate::shared::Context,
    image: Option<&str>,
) -> Result<(), anyhow::Error> {
    if let Some(device) = ELGATO_DEVICES.read().await.get(&context.device) {
        if let Some(image) = image {
            let dyn_img = load_dynamic_image(image).await?;

            if context.controller == "Encoder" {
                device
                    .write_lcd(
                        (context.position as u16 * 200) + 64,
                        14,
                        &ImageRect::from_image_async(dyn_img.resize(
                            72,
                            72,
                            image::imageops::FilterType::Nearest,
                        ))?,
                    )
                    .await?;
            } else {
                device.set_button_image(context.position, dyn_img).await?;
            }
        } else if context.controller == "Encoder" {
            device
                .write_lcd(
                    context.position as u16 * 200,
                    0,
                    &ImageRect::from_image_async(image::DynamicImage::new_rgb8(200, 100))?,
                )
                .await?;
        } else {
            device.clear_button_image(context.position).await?;
        }
        device.flush().await?;
    }
    Ok(())
}

pub async fn set_lcd_background(id: &str, image: Option<&str>) -> Result<(), anyhow::Error> {
    if let Some(device) = ELGATO_DEVICES.read().await.get(id) {
        if device.kind() != Kind::Plus {
            return Ok(());
        }
        let fmt = device.kind().lcd_image_format().unwrap();
        let dyn_img = if let Some(image) = image {
            load_dynamic_image(image).await?.resize_exact(
                800,
                100,
                image::imageops::FilterType::Nearest,
            )
        } else {
            image::DynamicImage::new_rgb8(800, 100)
        };

        device
            .write_lcd_fill(&convert_image_with_format_async(fmt, dyn_img)?)
            .await?;
        device.flush().await?;
    }
    Ok(())
}

pub async fn clear_screen(id: &str) -> Result<(), anyhow::Error> {
    if let Some(device) = ELGATO_DEVICES.read().await.get(id) {
        device.clear_all_button_images().await?;
        if device.kind() == Kind::Plus {
            device
                .write_lcd_fill(&convert_image_with_format_async(
                    device.kind().lcd_image_format().unwrap(),
                    image::DynamicImage::new_rgb8(800, 100),
                )?)
                .await?;
        }
        device.flush().await?;
    }
    Ok(())
}

pub async fn set_brightness(brightness: u8) {
    for (_id, device) in ELGATO_DEVICES.read().await.iter() {
        let _ = device.set_brightness(brightness.clamp(0, 100)).await;
        let _ = device.flush().await;
    }
}

pub async fn reset_devices() {
    for (_id, device) in ELGATO_DEVICES.read().await.iter() {
        let _ = device.reset().await;
        let _ = device.flush().await;
    }
}

async fn init(device: AsyncStreamDeck, device_id: String) {
    if ELGATO_DEVICES.read().await.contains_key(&device_id) {
        return;
    }

    let kind = device.kind();
    let device_type = match kind {
        Kind::Original | Kind::OriginalV2 | Kind::Mk2 | Kind::Mk2Scissor | Kind::Mk2Module => 0,
        Kind::Mini | Kind::MiniMk2 | Kind::MiniDiscord | Kind::MiniMk2Module => 1,
        Kind::Xl | Kind::XlV2 | Kind::XlV2Module => 2,
        Kind::Pedal => 5,
        Kind::Plus => 7,
        Kind::Neo => 9,
    };
    let _ = device.clear_all_button_images().await;
    if let Ok(settings) = crate::store::get_settings() {
        let _ = device.set_brightness(settings.value.brightness).await;
    }
    let _ = device.flush().await;

    // IMPORTANT: register the physical device handle before we emit `willAppear`.
    // `register_device()` triggers `will_appear()` for each instance, which pushes initial
    // images to hardware via `elgato::update_image()`. If we haven't inserted the device yet,
    // those initial image writes are silently skipped.
    let name = device.product().await.unwrap();
    let reader = device.get_reader();
    ELGATO_DEVICES
        .write()
        .await
        .insert(device_id.clone(), device);

    crate::events::inbound::devices::register_device(
        "",
        crate::events::inbound::PayloadEvent {
            payload: crate::shared::DeviceInfo {
                id: device_id.clone(),
                plugin: String::new(),
                name,
                rows: kind.row_count(),
                columns: kind.column_count(),
                encoders: kind.encoder_count(),
                r#type: device_type,
                screen: if kind == Kind::Plus {
                    Some(crate::shared::DeviceScreenInfo {
                        width_px: 800,
                        height_px: 100,
                        segments: kind.encoder_count(),
                        placement: crate::shared::ScreenPlacement::BetweenKeypadAndEncoders,
                    })
                } else {
                    None
                },
            },
        },
    )
    .await
    .unwrap();

    loop {
        let updates = match reader.read(100.0).await {
            Ok(updates) => updates,
            Err(_) => break,
        };
        for update in updates {
            match match update {
                DeviceStateUpdate::ButtonDown(key) => keypad::key_down(&device_id, key).await,
                DeviceStateUpdate::ButtonUp(key) => keypad::key_up(&device_id, key).await,
                DeviceStateUpdate::EncoderTwist(dial, ticks) => {
                    encoder::dial_rotate(&device_id, dial, ticks.into()).await
                }
                DeviceStateUpdate::EncoderDown(dial) => {
                    encoder::dial_press(&device_id, "dialDown", dial).await
                }
                DeviceStateUpdate::EncoderUp(dial) => {
                    encoder::dial_press(&device_id, "dialUp", dial).await
                }
                _ => Ok(()),
            } {
                Ok(_) => (),
                Err(error) => log::warn!("Failed to process device event {update:?}: {error}"),
            }
        }
    }

    ELGATO_DEVICES.write().await.remove(&device_id);
    crate::events::inbound::devices::deregister_device(
        "",
        crate::events::inbound::PayloadEvent { payload: device_id },
    )
    .await
    .unwrap();
}

/// Attempt to initialise all connected devices.
pub async fn initialise_devices() {
    if let Ok(settings) = crate::store::get_settings() {
        if settings.value.disableelgato {
            crate::plugins::DEVICE_NAMESPACES.write().await.insert(
                "sd".to_owned(),
                "opendeck_alternative_elgato_implementation".to_owned(),
            );
            return;
        } else {
            crate::plugins::DEVICE_NAMESPACES.write().await.remove("sd");
        }
    }

    // Iterate through detected Elgato devices and attempt to register them.
    match elgato_streamdeck::new_hidapi() {
        Ok(hid) => {
            for (kind, serial) in elgato_streamdeck::asynchronous::list_devices_async(&hid) {
                let device_id = format!("sd-{serial}");
                if ELGATO_DEVICES.read().await.contains_key(&device_id) {
                    continue;
                }
                match elgato_streamdeck::AsyncStreamDeck::connect(&hid, kind, &serial) {
                    Ok(device) => {
                        tokio::spawn(init(device, device_id));
                    }
                    Err(error) => log::warn!("Failed to connect to Elgato device: {error}"),
                }
            }
        }
        Err(error) => log::warn!("Failed to initialise hidapi: {error}"),
    }
}
