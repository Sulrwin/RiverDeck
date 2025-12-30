//! Animated image support (currently: GIF).
//!
//! This module is intentionally backend-only: it prepares decoded frames for
//! device rendering and provides simple cancellation bookkeeping for running
//! animation tasks.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use image::AnimationDecoder as _;
use image::codecs::gif::GifDecoder;
use once_cell::sync::Lazy;
use tokio::sync::Mutex;

/// Hard safety limits to keep memory/CPU bounded.
const MAX_FRAMES: usize = 300;
const MIN_FRAME_DELAY: Duration = Duration::from_millis(33); // cap at ~30 FPS
const MAX_FRAME_DELAY: Duration = Duration::from_secs(2);
const MAX_CACHE_ENTRIES: usize = 32;

#[derive(Debug, Clone)]
pub struct PreparedFrame {
    pub delay: Duration,
    pub image: image::DynamicImage,
}

#[derive(Debug, Clone, Copy)]
pub enum ResizeMode {
    /// Exact resize to device target resolution (used for Plus LCD background).
    Exact,
    /// Best-effort resize that preserves aspect ratio within bounds.
    Fit,
}

#[derive(Debug, Clone, Copy)]
pub struct Target {
    pub width: u32,
    pub height: u32,
    pub resize_mode: ResizeMode,
    pub filter: image::imageops::FilterType,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AnimationKey {
    pub device: String,
    pub controller: String,
    pub position: u8,
}

#[derive(Default)]
struct AnimationRegistry {
    /// Monotonic generation per key; tasks should exit when their generation is no longer current.
    generation: HashMap<AnimationKey, u64>,
}

static REGISTRY: Lazy<Mutex<AnimationRegistry>> =
    Lazy::new(|| Mutex::new(AnimationRegistry::default()));

static GIF_CACHE: Lazy<Mutex<HashMap<String, Arc<Vec<PreparedFrame>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Bump the generation for an animation key and return the new generation.
pub async fn next_generation(key: &AnimationKey) -> u64 {
    let mut reg = REGISTRY.lock().await;
    let entry = reg.generation.entry(key.clone()).or_insert(0);
    *entry = entry.wrapping_add(1).max(1);
    *entry
}

/// Stop an animation key (increments generation so existing tasks self-terminate, then removes entry).
pub async fn stop(key: &AnimationKey) {
    let mut reg = REGISTRY.lock().await;
    if let Some(g) = reg.generation.get_mut(key) {
        *g = g.wrapping_add(1).max(1);
    }
    reg.generation.remove(key);
}

/// Check whether a generation is still current for a key.
pub async fn is_current(key: &AnimationKey, generation: u64) -> bool {
    let reg = REGISTRY.lock().await;
    reg.generation.get(key).copied().unwrap_or(0) == generation
}

/// Decode an animated GIF into a bounded list of frames.
pub fn decode_gif(bytes: &[u8]) -> Result<Vec<PreparedFrame>, anyhow::Error> {
    let decoder = GifDecoder::new(Cursor::new(bytes))?;
    let frames = decoder.into_frames();
    let frames: Vec<image::Frame> = frames.collect_frames()?;

    if frames.is_empty() {
        return Err(anyhow::anyhow!("gif has no frames"));
    }

    let mut out: Vec<PreparedFrame> = Vec::new();
    for frame in frames.into_iter().take(MAX_FRAMES) {
        // image::Frame uses Delay in (numerator, denominator) in ms? Convert via `numer_denom_ms`.
        let (num, den) = frame.delay().numer_denom_ms();
        let mut delay = if den == 0 {
            MIN_FRAME_DELAY
        } else {
            // `numer_denom_ms()` returns a fractional millisecond value (num / den ms).
            // Round to nearest millisecond.
            Duration::from_millis(
                ((num as u64).saturating_add((den as u64) / 2)).saturating_div(den as u64),
            )
        };
        delay = delay.clamp(MIN_FRAME_DELAY, MAX_FRAME_DELAY);

        let img = image::DynamicImage::ImageRgba8(frame.into_buffer());
        out.push(PreparedFrame { delay, image: img });
    }

    Ok(out)
}

/// Decode an animated GIF and cache the decoded base frames by `cache_key`.
pub async fn decode_gif_cached(
    cache_key: String,
    bytes: Vec<u8>,
) -> Result<Arc<Vec<PreparedFrame>>, anyhow::Error> {
    {
        let cache = GIF_CACHE.lock().await;
        if let Some(v) = cache.get(&cache_key) {
            return Ok(v.clone());
        }
    }

    let decoded = Arc::new(decode_gif(&bytes)?);
    let mut cache = GIF_CACHE.lock().await;
    // Simple bounded cache eviction: if over capacity, drop an arbitrary (oldest-unknown) entry.
    if cache.len() >= MAX_CACHE_ENTRIES
        && let Some(k) = cache.keys().next().cloned()
    {
        cache.remove(&k);
    }
    cache.insert(cache_key, decoded.clone());
    Ok(decoded)
}

/// Prepare a decoded GIF for a specific device target size.
pub fn prepare_frames(
    frames: &[PreparedFrame],
    target: Target,
    overlays: Option<&[crate::shared::LabelOverlay]>,
    overlay_fn: Option<
        fn(image::DynamicImage, &crate::shared::LabelOverlay) -> image::DynamicImage,
    >,
) -> Vec<PreparedFrame> {
    let mut out: Vec<PreparedFrame> = Vec::with_capacity(frames.len());
    for f in frames {
        let mut f = f.clone();
        let mut img = match target.resize_mode {
            ResizeMode::Exact => f
                .image
                .resize_exact(target.width, target.height, target.filter),
            ResizeMode::Fit => f.image.resize(target.width, target.height, target.filter),
        };

        if let (Some(ov), Some(apply)) = (overlays, overlay_fn) {
            for o in ov {
                if !o.text.trim().is_empty() {
                    img = apply(img, o);
                }
            }
        }

        f.image = img;
        out.push(f);
    }
    out
}
