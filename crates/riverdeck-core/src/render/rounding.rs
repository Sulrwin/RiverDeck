use image::{DynamicImage, Rgba, RgbaImage};

/// Apply a subtle rounded-corner alpha mask to an image.
///
/// This is used for keypad button images so icons feel a bit less harsh.
/// The radius is scaled from the image size so it stays "tiny" across devices:
/// ~3px at 72x72, proportionally larger for bigger keys.
pub fn round_corners_subtle(img: DynamicImage) -> DynamicImage {
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    if w == 0 || h == 0 {
        return DynamicImage::ImageRgba8(rgba);
    }

    // ~3px radius at 72x72, scaled with min side. Clamp to keep it subtle.
    let min_side = w.min(h).max(1);
    let mut radius = ((min_side as f32) * (3.0 / 72.0)).round() as i32;
    radius = radius.clamp(1, (min_side / 2) as i32);
    DynamicImage::ImageRgba8(apply_round_corners_alpha(rgba, radius as u32, true))
}

/// Apply a rounded-rect alpha mask with the given `radius_px`.
///
/// If `antialias_1px` is true, adds a 1px soft edge on the corner arc.
pub fn apply_round_corners_alpha(
    mut img: RgbaImage,
    radius_px: u32,
    antialias_1px: bool,
) -> RgbaImage {
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return img;
    }
    let r = radius_px.min(w / 2).min(h / 2);
    if r == 0 {
        return img;
    }

    // Only touch the four corner squares; keep it cheap.
    // We compute coverage based on distance from the rounded corner center.
    let r_f = r as f32;
    let aa = if antialias_1px { 1.0 } else { 0.0 };
    let outer = r_f;
    let inner = (r_f - aa).max(0.0);
    let outer2 = outer * outer;
    let inner2 = inner * inner;

    fn scale_alpha(px: &mut Rgba<u8>, factor: f32) {
        if px[3] == 0 {
            return;
        }
        let a = (px[3] as f32) * factor;
        px[3] = a.round().clamp(0.0, 255.0) as u8;
    }

    let mut apply_corner = |x0: u32, y0: u32, cx: f32, cy: f32| {
        for y in 0..r {
            for x in 0..r {
                let px_x = x0 + x;
                let px_y = y0 + y;
                if px_x >= w || px_y >= h {
                    continue;
                }
                let dx = (px_x as f32 + 0.5) - cx;
                let dy = (px_y as f32 + 0.5) - cy;
                let d2 = dx * dx + dy * dy;

                if d2 <= inner2 {
                    continue; // fully inside rounded area
                }
                let p = img.get_pixel_mut(px_x, px_y);
                if d2 >= outer2 {
                    p[3] = 0; // outside
                } else if antialias_1px && outer2 > inner2 {
                    // Linear ramp between inner and outer radii.
                    let d = d2.sqrt();
                    let t = (outer - d) / (outer - inner);
                    scale_alpha(p, t.clamp(0.0, 1.0));
                }
            }
        }
    };

    // Top-left
    apply_corner(0, 0, r_f, r_f);
    // Top-right
    apply_corner(w.saturating_sub(r), 0, (w as f32) - r_f, r_f);
    // Bottom-left
    apply_corner(0, h.saturating_sub(r), r_f, (h as f32) - r_f);
    // Bottom-right
    apply_corner(
        w.saturating_sub(r),
        h.saturating_sub(r),
        (w as f32) - r_f,
        (h as f32) - r_f,
    );

    img
}
