use crate::events::outbound::{encoder, keypad};

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use elgato_streamdeck::{
    AsyncStreamDeck, DeviceStateUpdate,
    images::{ImageRect, convert_image_with_format_async},
    info::Kind,
};
use image::{Rgba, RgbaImage};
use once_cell::sync::Lazy;
use tokio::sync::{Mutex, RwLock};
use tokio::time::sleep;

static ELGATO_DEVICES: Lazy<RwLock<HashMap<String, AsyncStreamDeck>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

fn env_truthy_any(name: &str) -> bool {
    matches!(
        std::env::var(name).ok().as_deref(),
        Some("1")
            | Some("true")
            | Some("TRUE")
            | Some("yes")
            | Some("YES")
            | Some("on")
            | Some("ON")
    )
}

fn spawn_all_test_devices_enabled() -> bool {
    // A single opt-in switch that we can flip from VSCode launch configs or shell env.
    // Supported values:
    // - "all" (recommended)
    // - truthy values ("1"/"true"/"yes"/"on")
    match std::env::var("RIVERDECK_TEST_DEVICES").ok() {
        Some(v) if v.trim().eq_ignore_ascii_case("all") => true,
        _ => env_truthy_any("RIVERDECK_TEST_DEVICES"),
    }
}

async fn register_spawn_all_test_devices() {
    use crate::events::inbound::PayloadEvent;

    // Representative devices for each supported family (distinct SDK `type` values).
    // Keep IDs stable so their profile/config stores remain stable across restarts.
    let devices: Vec<(Kind, &'static str, &'static str, u8)> = vec![
        (Kind::Mk2, "mk2", "Stream Deck MK.2", 0),
        (Kind::MiniMk2, "mini", "Stream Deck Mini", 1),
        (Kind::XlV2, "xl", "Stream Deck XL", 2),
        (Kind::Pedal, "pedal", "Stream Deck Pedal", 5),
        (Kind::Plus, "plus", "Stream Deck +", 7),
        (Kind::Neo, "neo", "Stream Deck Neo", 9),
    ];

    for (kind, suffix, display_name, device_type) in devices {
        let id = format!("sd-test-{suffix}");
        if crate::shared::DEVICES.contains_key(&id) {
            continue;
        }

        let screen = if kind == Kind::Plus {
            Some(crate::shared::DeviceScreenInfo {
                width_px: 800,
                height_px: 100,
                segments: kind.encoder_count(),
                placement: crate::shared::ScreenPlacement::BetweenKeypadAndEncoders,
            })
        } else {
            None
        };

        let info = crate::shared::DeviceInfo {
            id,
            plugin: String::new(),
            name: format!("(Test) {display_name}"),
            rows: kind.row_count(),
            columns: kind.column_count(),
            encoders: kind.encoder_count(),
            r#type: device_type,
            screen,
        };

        let _ =
            crate::events::inbound::devices::register_device("", PayloadEvent { payload: info })
                .await;
    }
}

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

#[derive(Clone)]
enum PlusLayer {
    None,
    Static(image::DynamicImage),
    Animated {
        frames: Vec<crate::animation::PreparedFrame>,
        idx: usize,
        next_at: Instant,
    },
}

impl PlusLayer {
    fn is_animated(&self) -> bool {
        matches!(self, PlusLayer::Animated { .. })
    }

    fn current_image(&self) -> image::DynamicImage {
        match self {
            PlusLayer::None => image::DynamicImage::ImageRgba8(RgbaImage::new(1, 1)),
            PlusLayer::Static(img) => img.clone(),
            PlusLayer::Animated { frames, idx, .. } => frames
                .get(*idx % frames.len())
                .map(|f| f.image.clone())
                .unwrap_or_else(|| image::DynamicImage::ImageRgba8(RgbaImage::new(1, 1))),
        }
    }
}

struct PlusDeviceState {
    generation: u64,
    background: PlusLayer, // 800x100
    dials: Vec<PlusLayer>, // per encoder, each 72x72 (or None)
    task_running: bool,
}

static PLUS_STATE: Lazy<RwLock<HashMap<String, Arc<Mutex<PlusDeviceState>>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

async fn plus_state_for_device(device_id: &str, encoders: usize) -> Arc<Mutex<PlusDeviceState>> {
    if let Some(st) = PLUS_STATE.read().await.get(device_id) {
        return st.clone();
    }
    let mut map = PLUS_STATE.write().await;
    map.entry(device_id.to_owned())
        .or_insert_with(|| {
            Arc::new(Mutex::new(PlusDeviceState {
                generation: 1,
                background: PlusLayer::None,
                dials: vec![PlusLayer::None; encoders],
                task_running: false,
            }))
        })
        .clone()
}

fn plus_bump_generation(state: &mut PlusDeviceState) -> u64 {
    state.generation = state.generation.wrapping_add(1).max(1);
    // Force re-spawn of the render task for the new generation.
    state.task_running = false;
    state.generation
}

fn plus_composite_frame(
    bg: &PlusLayer,
    dials: &[(u8, image::DynamicImage)],
) -> image::DynamicImage {
    // Compose into a single RGBA image then rely on elgato-streamdeck conversion.
    let mut base = match bg {
        PlusLayer::None => RgbaImage::from_pixel(800, 100, Rgba([0, 0, 0, 255])),
        _ => bg
            .current_image()
            .resize_exact(800, 100, image::imageops::FilterType::Nearest)
            .to_rgba8(),
    };

    for (dial, icon) in dials {
        let icon = icon
            .resize_exact(72, 72, image::imageops::FilterType::Nearest)
            .to_rgba8();
        let ox = (*dial as u32) * 200 + 64;
        let oy = 14u32;
        for y in 0..icon.height() {
            for x in 0..icon.width() {
                let dst_x = ox + x;
                let dst_y = oy + y;
                if dst_x < base.width() && dst_y < base.height() {
                    let src = *icon.get_pixel(x, y);
                    if src[3] != 0 {
                        let dst = base.get_pixel_mut(dst_x, dst_y);
                        blend_pixel(dst, src);
                    }
                }
            }
        }
    }

    image::DynamicImage::ImageRgba8(base)
}

async fn plus_render_once(device_id: &str, state: Arc<Mutex<PlusDeviceState>>) {
    let device = {
        let devices = ELGATO_DEVICES.read().await;
        devices.get(device_id).cloned()
    };
    let Some(device) = device else { return };
    if device.kind() != Kind::Plus {
        return;
    }

    // Snapshot current images without holding the state lock across awaits.
    let (bg, dial_imgs) = {
        let st = state.lock().await;
        let bg = st.background.clone();
        let mut out: Vec<(u8, image::DynamicImage)> = Vec::new();
        for (idx, layer) in st.dials.iter().enumerate() {
            match layer {
                PlusLayer::None => {}
                _ => out.push((idx as u8, layer.current_image())),
            }
        }
        (bg, out)
    };

    let composed = plus_composite_frame(&bg, &dial_imgs);
    let fmt = device.kind().lcd_image_format().unwrap();
    if let Ok(buf) = convert_image_with_format_async(fmt, composed) {
        let _ = device.write_lcd_fill(&buf).await;
        let _ = device.flush().await;
    }
}

async fn plus_ensure_task(device_id: String, state: Arc<Mutex<PlusDeviceState>>, generation: u64) {
    // Avoid spawning multiple tasks.
    {
        let mut st = state.lock().await;
        if st.task_running && st.generation == generation {
            return;
        }
        st.task_running = true;
    }

    tokio::spawn(async move {
        loop {
            // Snapshot current state and also advance any due animations.
            let (still_current, any_animated, sleep_for) = {
                let mut st = state.lock().await;
                if st.generation != generation {
                    st.task_running = false;
                    (false, false, Duration::from_millis(0))
                } else {
                    let now = Instant::now();
                    let mut next: Option<Instant> = None;

                    let mut bump_layer = |layer: &mut PlusLayer| {
                        let PlusLayer::Animated {
                            frames,
                            idx,
                            next_at,
                        } = layer
                        else {
                            return;
                        };
                        if frames.is_empty() {
                            return;
                        }
                        if *next_at <= now {
                            *idx = idx.wrapping_add(1) % frames.len();
                            *next_at = now + frames[*idx].delay;
                        }
                        next = Some(next.map(|n| n.min(*next_at)).unwrap_or(*next_at));
                    };

                    bump_layer(&mut st.background);
                    for dial in st.dials.iter_mut() {
                        bump_layer(dial);
                    }

                    let any =
                        st.background.is_animated() || st.dials.iter().any(|d| d.is_animated());
                    let sleep_for = if any {
                        next.and_then(|t| t.checked_duration_since(Instant::now()))
                            .unwrap_or_else(|| Duration::from_millis(5))
                            .clamp(Duration::from_millis(1), Duration::from_millis(200))
                    } else {
                        Duration::from_millis(0)
                    };
                    (true, any, sleep_for)
                }
            };

            if !still_current {
                break;
            }

            // Render current frame (background + dial overlays).
            plus_render_once(&device_id, state.clone()).await;

            if !any_animated {
                // No ongoing animation; shut down.
                let mut st = state.lock().await;
                st.task_running = false;
                break;
            }

            sleep(sleep_for).await;
        }
    });
}

use crate::render::label::overlay_label;

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

async fn resolve_image_bytes(image: &str) -> Result<Vec<u8>, anyhow::Error> {
    if image.trim().starts_with("data:") {
        // Stream Deck SDK commonly uses data URLs.
        // Support both base64 and "raw" (non-base64) payloads.
        if image.contains(";base64,") {
            let (_meta, b64) = image
                .split_once(";base64,")
                .ok_or_else(|| anyhow::anyhow!("invalid data url (missing ';base64,')"))?;
            Ok(base64::engine::general_purpose::STANDARD.decode(b64)?)
        } else {
            let (_meta, raw) = image
                .split_once(',')
                .ok_or_else(|| anyhow::anyhow!("invalid data url (missing ',')"))?;
            Ok(raw.as_bytes().to_vec())
        }
    } else {
        // Mirror `load_dynamic_image` path resolution so plugins can animate from the same sources.
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
        Ok(tokio::fs::read(path).await?)
    }
}

fn is_gif(bytes: &[u8]) -> bool {
    bytes.len() >= 6 && &bytes[0..3] == b"GIF"
}

fn gif_cache_key(bytes: &[u8]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    format!("gif:{:016x}", h.finish())
}

pub async fn update_image(
    context: &crate::shared::Context,
    image: Option<&str>,
    overlays: Option<Vec<(String, crate::shared::TextPlacement)>>,
) -> Result<(), anyhow::Error> {
    if let Some(device) = ELGATO_DEVICES.read().await.get(&context.device) {
        let anim_key = crate::animation::AnimationKey {
            device: context.device.clone(),
            controller: context.controller.clone(),
            position: context.position,
        };
        if let Some(image) = image {
            let bytes = resolve_image_bytes(image).await;
            if let Ok(bytes) = bytes
                && is_gif(&bytes)
            {
                // Animated GIF: stop any existing animation for this slot and start a new one.
                let generation = crate::animation::next_generation(&anim_key).await;
                let cache_key = gif_cache_key(&bytes);
                let decoded = crate::animation::decode_gif_cached(cache_key, bytes).await?;

                // Single-frame GIFs behave like static images.
                if decoded.len() <= 1 {
                    crate::animation::stop(&anim_key).await;
                } else if context.controller == "Encoder" {
                    // Stream Deck Plus: encoder icons are drawn on the shared 800x100 LCD.
                    // We must render them via the compositor so animated backgrounds and dial icons
                    // can coexist.
                    if device.kind() == Kind::Plus {
                        // Ensure we don't have any legacy per-context encoder animation task running.
                        crate::animation::stop(&anim_key).await;

                        let state = plus_state_for_device(
                            &context.device,
                            device.kind().encoder_count() as usize,
                        )
                        .await;
                        let generation = {
                            let mut st = state.lock().await;
                            if st.dials.len() < device.kind().encoder_count() as usize {
                                st.dials.resize(
                                    device.kind().encoder_count() as usize,
                                    PlusLayer::None,
                                );
                            }
                            if decoded.len() <= 1 {
                                // Single-frame GIF: treat as static.
                                let img = decoded
                                    .first()
                                    .map(|f| f.image.clone())
                                    .unwrap_or_else(|| image::DynamicImage::new_rgba8(72, 72));
                                let img =
                                    img.resize_exact(72, 72, image::imageops::FilterType::Nearest);
                                let mut img = img;
                                if let Some(ov) = overlays.as_deref() {
                                    for (label, placement) in ov {
                                        if !label.trim().is_empty() {
                                            img = overlay_label(img, label, *placement);
                                        }
                                    }
                                }
                                st.dials[context.position as usize] = PlusLayer::Static(img);
                            } else {
                                let prepared = crate::animation::prepare_frames(
                                    decoded.as_ref(),
                                    crate::animation::Target {
                                        width: 72,
                                        height: 72,
                                        resize_mode: crate::animation::ResizeMode::Exact,
                                        filter: image::imageops::FilterType::Nearest,
                                    },
                                    overlays.as_deref(),
                                    Some(overlay_label),
                                );
                                let next_at = Instant::now() + prepared[0].delay;
                                st.dials[context.position as usize] = PlusLayer::Animated {
                                    frames: prepared,
                                    idx: 0,
                                    next_at,
                                };
                            }
                            plus_bump_generation(&mut st)
                        };

                        plus_render_once(&context.device, state.clone()).await;
                        let any_animated = {
                            let st = state.lock().await;
                            st.background.is_animated() || st.dials.iter().any(|d| d.is_animated())
                        };
                        if any_animated {
                            plus_ensure_task(context.device.clone(), state, generation).await;
                        }
                        return Ok(());
                    }

                    // Non-Plus encoder LCD icons are 72x72.
                    // Apply overlay *after* resize for crisp 8x8 text.
                    let prepared = crate::animation::prepare_frames(
                        decoded.as_ref(),
                        crate::animation::Target {
                            width: 72,
                            height: 72,
                            resize_mode: crate::animation::ResizeMode::Fit,
                            filter: image::imageops::FilterType::Nearest,
                        },
                        overlays.as_deref(),
                        Some(overlay_label),
                    );

                    // Render first frame immediately.
                    device
                        .write_lcd(
                            (context.position as u16 * 200) + 64,
                            14,
                            &ImageRect::from_image_async(prepared[0].image.clone())?,
                        )
                        .await?;
                    device.flush().await?;

                    let device_id = context.device.clone();
                    let controller = context.controller.clone();
                    let position = context.position;
                    tokio::spawn(async move {
                        let mut idx = 1usize;
                        loop {
                            if !crate::animation::is_current(&anim_key, generation).await {
                                break;
                            }
                            let device = {
                                let devices = ELGATO_DEVICES.read().await;
                                devices.get(&device_id).cloned()
                            };
                            let Some(device) = device else { break };

                            let frame = &prepared[idx % prepared.len()];
                            if controller == "Encoder" {
                                let rect = match ImageRect::from_image_async(frame.image.clone()) {
                                    Ok(r) => r,
                                    Err(_) => {
                                        sleep(frame.delay).await;
                                        idx = idx.wrapping_add(1);
                                        continue;
                                    }
                                };
                                let _ = device
                                    .write_lcd((position as u16 * 200) + 64, 14, &rect)
                                    .await;
                                let _ = device.flush().await;
                            }
                            sleep(frame.delay).await;
                            idx = idx.wrapping_add(1);
                        }
                    });

                    return Ok(());
                } else {
                    // Keypad GIF: pre-resize frames to the device key resolution, then apply overlays.
                    // This keeps text sizing consistent across keys regardless of the source GIF dimensions.
                    let (kw, kh) = device.kind().key_image_format().size;
                    let prepared = crate::animation::prepare_frames(
                        decoded.as_ref(),
                        crate::animation::Target {
                            width: kw as u32,
                            height: kh as u32,
                            resize_mode: crate::animation::ResizeMode::Exact,
                            filter: image::imageops::FilterType::Nearest,
                        },
                        overlays.as_deref(),
                        Some(overlay_label),
                    );

                    // Render first frame immediately.
                    if let Err(e) = device
                        .set_button_image(context.position, prepared[0].image.clone())
                        .await
                    {
                        log::warn!("Failed to render first GIF frame: {}", e);
                    }
                    let _ = device.flush().await;

                    let device_id = context.device.clone();
                    let position = context.position;
                    tokio::spawn(async move {
                        let mut idx = 1usize;
                        loop {
                            if !crate::animation::is_current(&anim_key, generation).await {
                                break;
                            }
                            let device = {
                                let devices = ELGATO_DEVICES.read().await;
                                devices.get(&device_id).cloned()
                            };
                            let Some(device) = device else { break };
                            let frame = &prepared[idx % prepared.len()];
                            let _ = device.set_button_image(position, frame.image.clone()).await;
                            let _ = device.flush().await;
                            sleep(frame.delay).await;
                            idx = idx.wrapping_add(1);
                        }
                    });

                    return Ok(());
                }
            }

            // Not an animated GIF (or failed to load bytes): treat as static.
            crate::animation::stop(&anim_key).await;
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
                if device.kind() == Kind::Plus {
                    let state = plus_state_for_device(
                        &context.device,
                        device.kind().encoder_count() as usize,
                    )
                    .await;
                    let generation = {
                        let mut st = state.lock().await;
                        if st.dials.len() < device.kind().encoder_count() as usize {
                            st.dials
                                .resize(device.kind().encoder_count() as usize, PlusLayer::None);
                        }
                        st.dials[context.position as usize] = PlusLayer::Static(final_img);
                        plus_bump_generation(&mut st)
                    };
                    plus_render_once(&context.device, state.clone()).await;
                    let any_animated = {
                        let st = state.lock().await;
                        st.background.is_animated() || st.dials.iter().any(|d| d.is_animated())
                    };
                    if any_animated {
                        plus_ensure_task(context.device.clone(), state, generation).await;
                    }
                } else {
                    device
                        .write_lcd(
                            (context.position as u16 * 200) + 64,
                            14,
                            &ImageRect::from_image_async(final_img)?,
                        )
                        .await?;
                }
            } else {
                // Keypad buttons: if we draw overlays before the elgato-streamdeck conversion,
                // and the source image isn't already at native key resolution, the subsequent
                // resize will also shrink our rendered text, making font sizes inconsistent
                // across keys (and sometimes unreadably small). To avoid this, resize first.
                let (kw, kh) = device.kind().key_image_format().size;
                let mut final_img = dyn_img.resize_exact(
                    kw as u32,
                    kh as u32,
                    image::imageops::FilterType::Nearest,
                );
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
            crate::animation::stop(&anim_key).await;
            if device.kind() == Kind::Plus {
                let state =
                    plus_state_for_device(&context.device, device.kind().encoder_count() as usize)
                        .await;
                let generation = {
                    let mut st = state.lock().await;
                    if st.dials.len() < device.kind().encoder_count() as usize {
                        st.dials
                            .resize(device.kind().encoder_count() as usize, PlusLayer::None);
                    }
                    st.dials[context.position as usize] = PlusLayer::None;
                    plus_bump_generation(&mut st)
                };
                plus_render_once(&context.device, state.clone()).await;
                let any_animated = {
                    let st = state.lock().await;
                    st.background.is_animated() || st.dials.iter().any(|d| d.is_animated())
                };
                if any_animated {
                    plus_ensure_task(context.device.clone(), state, generation).await;
                }
            } else {
                device
                    .write_lcd(
                        context.position as u16 * 200,
                        0,
                        &ImageRect::from_image_async(image::DynamicImage::new_rgb8(200, 100))?,
                    )
                    .await?;
            }
        } else {
            crate::animation::stop(&anim_key).await;
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
        let state = plus_state_for_device(id, device.kind().encoder_count() as usize).await;

        let (generation, any_animated) = if let Some(image) = image {
            // Prefer decoding from bytes so GIF detection works for both `data:` and file paths.
            let bytes = resolve_image_bytes(image).await?;
            if is_gif(&bytes) {
                let decoded =
                    crate::animation::decode_gif_cached(gif_cache_key(&bytes), bytes).await?;
                let mut st = state.lock().await;
                if decoded.len() <= 1 {
                    st.background = PlusLayer::Static(
                        decoded
                            .first()
                            .map(|f| f.image.clone())
                            .unwrap_or_else(|| image::DynamicImage::new_rgb8(800, 100))
                            .resize_exact(800, 100, image::imageops::FilterType::Nearest),
                    );
                } else {
                    let prepared = crate::animation::prepare_frames(
                        decoded.as_ref(),
                        crate::animation::Target {
                            width: 800,
                            height: 100,
                            resize_mode: crate::animation::ResizeMode::Exact,
                            filter: image::imageops::FilterType::Nearest,
                        },
                        None,
                        None,
                    );
                    let next_at = Instant::now() + prepared[0].delay;
                    st.background = PlusLayer::Animated {
                        frames: prepared,
                        idx: 0,
                        next_at,
                    };
                }
                let generation = plus_bump_generation(&mut st);
                let any = st.background.is_animated() || st.dials.iter().any(|d| d.is_animated());
                (generation, any)
            } else {
                let dyn_img = image::load_from_memory(&bytes)?.resize_exact(
                    800,
                    100,
                    image::imageops::FilterType::Nearest,
                );
                let mut st = state.lock().await;
                st.background = PlusLayer::Static(dyn_img);
                let generation = plus_bump_generation(&mut st);
                let any = st.dials.iter().any(|d| d.is_animated());
                (generation, any)
            }
        } else {
            let mut st = state.lock().await;
            st.background = PlusLayer::None;
            let generation = plus_bump_generation(&mut st);
            let any = st.dials.iter().any(|d| d.is_animated());
            (generation, any)
        };

        // Render immediately; if any layer is animated, ensure the compositor task is running.
        plus_render_once(id, state.clone()).await;
        if any_animated {
            plus_ensure_task(id.to_owned(), state, generation).await;
        }
    }
    Ok(())
}

pub async fn clear_screen(id: &str) -> Result<(), anyhow::Error> {
    if let Some(device) = ELGATO_DEVICES.read().await.get(id) {
        device.clear_all_button_images().await?;
        if device.kind() == Kind::Plus {
            let state = plus_state_for_device(id, device.kind().encoder_count() as usize).await;
            {
                let mut st = state.lock().await;
                st.background = PlusLayer::None;
                for dial in st.dials.iter_mut() {
                    *dial = PlusLayer::None;
                }
                let _ = plus_bump_generation(&mut st);
            }
            plus_render_once(id, state).await;
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
                DeviceStateUpdate::TouchScreenPress(x, y) => {
                    encoder::touch_tap(&device_id, x, y, false).await
                }
                DeviceStateUpdate::TouchScreenLongPress(x, y) => {
                    encoder::touch_tap(&device_id, x, y, true).await
                }
                // Touch points are low-level segment state changes on devices like Stream Deck+.
                // The official Stream Deck SDK surface is the `touchTap` event (with `hold`),
                // which we emit from the higher-level TouchScreenPress/LongPress updates above.
                DeviceStateUpdate::TouchPointDown(_) | DeviceStateUpdate::TouchPointUp(_) => Ok(()),
                DeviceStateUpdate::TouchScreenSwipe(_, _) => Ok(()),
            } {
                Ok(_) => (),
                Err(error) => log::warn!("Failed to process device event {update:?}: {error}"),
            }
        }
    }

    ELGATO_DEVICES.write().await.remove(&device_id);
    PLUS_STATE.write().await.remove(&device_id);
    crate::events::inbound::devices::deregister_device(
        "",
        crate::events::inbound::PayloadEvent { payload: device_id },
    )
    .await
    .unwrap();
}

/// Attempt to initialise all connected devices.
pub async fn initialise_devices() {
    if spawn_all_test_devices_enabled() {
        register_spawn_all_test_devices().await;
    }

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
