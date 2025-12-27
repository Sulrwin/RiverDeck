use crate::events::outbound::{encoder, keypad};

use std::collections::HashMap;
use std::path::Path;

use base64::Engine as _;
use elgato_streamdeck::{
    AsyncStreamDeck, DeviceStateUpdate,
    images::{ImageRect, convert_image_with_format_async},
    info::Kind,
};
use font8x8::UnicodeFonts;
use image::{Rgba, RgbaImage};
use once_cell::sync::Lazy;
use tokio::sync::RwLock;

static ELGATO_DEVICES: Lazy<RwLock<HashMap<String, AsyncStreamDeck>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

fn blend_pixel(dst: &mut Rgba<u8>, src: Rgba<u8>) {
    let sa = src[3] as f32 / 255.0;
    if sa <= 0.0 {
        return;
    }
    let da = dst[3] as f32 / 255.0;
    let out_a = sa + da * (1.0 - sa);
    if out_a <= 0.0 {
        *dst = Rgba([0, 0, 0, 0]);
        return;
    }
    let blend = |sc: u8, dc: u8| -> u8 {
        let sc = sc as f32 / 255.0;
        let dc = dc as f32 / 255.0;
        let out_c = (sc * sa + dc * da * (1.0 - sa)) / out_a;
        (out_c * 255.0).round().clamp(0.0, 255.0) as u8
    };
    dst[0] = blend(src[0], dst[0]);
    dst[1] = blend(src[1], dst[1]);
    dst[2] = blend(src[2], dst[2]);
    dst[3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
}

fn draw_rect(img: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32, color: Rgba<u8>) {
    let iw = img.width();
    let ih = img.height();
    let x1 = (x + w).min(iw);
    let y1 = (y + h).min(ih);
    for yy in y..y1 {
        for xx in x..x1 {
            let p = img.get_pixel_mut(xx, yy);
            blend_pixel(p, color);
        }
    }
}

fn draw_text_8x8(img: &mut RgbaImage, x: u32, y: u32, text: &str, scale: u32, color: Rgba<u8>) {
    let scale = scale.max(1);
    let mut cursor_x = x;
    for ch in text.chars() {
        if ch == '\n' {
            break;
        }
        let glyph = font8x8::BASIC_FONTS.get(ch).unwrap_or([0u8; 8]);
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..8 {
                if (bits >> col) & 1 == 1 {
                    let px = cursor_x + (7 - col) as u32 * scale;
                    let py = y + row as u32 * scale;
                    for dy in 0..scale {
                        for dx in 0..scale {
                            if px + dx < img.width() && py + dy < img.height() {
                                let p = img.get_pixel_mut(px + dx, py + dy);
                                blend_pixel(p, color);
                            }
                        }
                    }
                }
            }
        }
        cursor_x += 8 * scale + scale; // 1px spacing (scaled)
    }
}

fn overlay_label(
    base: image::DynamicImage,
    label: &str,
    placement: crate::shared::TextPlacement,
) -> image::DynamicImage {
    let label = label.trim();
    if label.is_empty() {
        return base;
    }

    let mut img = base.to_rgba8();
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return image::DynamicImage::ImageRgba8(img);
    }

    // Choose a scale that stays readable across common Stream Deck sizes.
    let min_side = w.min(h).max(1);
    let scale = (min_side / 72).clamp(1, 3);

    // Truncate to fit (horizontal).
    let max_w = w.saturating_sub(8);
    let char_w = 8 * scale + scale;
    let mut text = label.to_owned();
    if (text.chars().count() as u32) * char_w > max_w && char_w > 0 {
        let max_chars = (max_w / char_w).saturating_sub(3) as usize;
        if max_chars > 0 {
            text = text.chars().take(max_chars).collect::<String>() + "...";
        } else {
            text = "...".to_owned();
        }
    }

    let text_w = (text.chars().count() as u32) * char_w;
    let text_h = 8 * scale;

    let mut text_img = RgbaImage::new(text_w.max(1), text_h.max(1));
    // Background strip for contrast.
    let bg_w = text_img.width();
    let bg_h = text_img.height();
    draw_rect(&mut text_img, 0, 0, bg_w, bg_h, Rgba([0, 0, 0, 140]));
    // Shadow + text.
    draw_text_8x8(
        &mut text_img,
        scale,
        scale,
        &text,
        scale,
        Rgba([0, 0, 0, 220]),
    );
    draw_text_8x8(
        &mut text_img,
        0,
        0,
        &text,
        scale,
        Rgba([255, 255, 255, 255]),
    );

    let (overlay, ox, oy) = match placement {
        crate::shared::TextPlacement::Top => {
            let x = (w.saturating_sub(text_img.width())) / 2;
            (text_img, x, 2)
        }
        crate::shared::TextPlacement::Bottom => {
            let x = (w.saturating_sub(text_img.width())) / 2;
            let y = h.saturating_sub(text_img.height() + 2);
            (text_img, x, y)
        }
        crate::shared::TextPlacement::Left => {
            let rot = image::imageops::rotate270(&text_img);
            let y = (h.saturating_sub(rot.height())) / 2;
            (rot, 2, y)
        }
        crate::shared::TextPlacement::Right => {
            let rot = image::imageops::rotate90(&text_img);
            let x = w.saturating_sub(rot.width() + 2);
            let y = (h.saturating_sub(rot.height())) / 2;
            (rot, x, y)
        }
    };

    // Composite overlay.
    for yy in 0..overlay.height() {
        for xx in 0..overlay.width() {
            let dst_x = ox + xx;
            let dst_y = oy + yy;
            if dst_x < w && dst_y < h {
                let src = *overlay.get_pixel(xx, yy);
                if src[3] != 0 {
                    let dst = img.get_pixel_mut(dst_x, dst_y);
                    blend_pixel(dst, src);
                }
            }
        }
    }

    image::DynamicImage::ImageRgba8(img)
}

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
    overlays: Option<Vec<(String, crate::shared::TextPlacement)>>,
) -> Result<(), anyhow::Error> {
    if let Some(device) = ELGATO_DEVICES.read().await.get(&context.device) {
        if let Some(image) = image {
            let dyn_img = load_dynamic_image(image).await?;

            if context.controller == "Encoder" {
                // For the encoder LCD, we draw icons at 72x72; overlay after resizing for sharper text.
                let mut final_img = dyn_img.resize(72, 72, image::imageops::FilterType::Nearest);
                if let Some(overlays) = overlays {
                    for (label, placement) in overlays {
                        if !label.trim().is_empty() {
                            final_img = overlay_label(final_img, &label, placement);
                        }
                    }
                }
                device
                    .write_lcd(
                        (context.position as u16 * 200) + 64,
                        14,
                        &ImageRect::from_image_async(final_img)?,
                    )
                    .await?;
            } else {
                // Apply text overlays directly to the image. The elgato-streamdeck crate handles
                // any necessary image format conversion and resizing internally.
                let mut final_img = dyn_img;
                if let Some(overlays) = overlays {
                    for (label, placement) in overlays {
                        if !label.trim().is_empty() {
                            final_img = overlay_label(final_img, &label, placement);
                        }
                    }
                }
                device.set_button_image(context.position, final_img).await?;
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
