use crate::render::label::overlay_label;
use crate::shared::LabelOverlay;

use image::{Rgba, RgbaImage};

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

/// Create a Stream Deck+ LCD strip segment overlay.
///
/// The physical strip is 800×100 split into 4 segments of 200×100. The dial “icon” is rendered
/// into the segment at (64,14) with size 72×72, and any label overlays are applied on top.
///
/// The returned image is **RGBA** with transparency so it can be composited over a background.
pub fn make_segment_overlay(
    icon: Option<image::DynamicImage>,
    overlays: Option<&[LabelOverlay]>,
) -> image::DynamicImage {
    // Transparent overlay so the background can show through.
    let mut base = RgbaImage::from_pixel(200, 100, Rgba([0, 0, 0, 0]));

    // Icon is a 72x72 square placed in the segment (matches device placement).
    if let Some(icon) = icon {
        let icon = icon
            .resize_exact(72, 72, image::imageops::FilterType::Nearest)
            .to_rgba8();
        let ox = 64u32;
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

    let mut out = image::DynamicImage::ImageRgba8(base);
    if let Some(ov) = overlays {
        for o in ov {
            if !o.text.trim().is_empty() {
                out = overlay_label(out, o);
            }
        }
    }
    out
}


