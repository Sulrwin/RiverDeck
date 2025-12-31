use image::{Rgba, RgbaImage};

use font8x8::UnicodeFonts;

use crate::shared::{LabelOverlay, TextPlacement};

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
                    // `font8x8` stores glyph bits LSB-first (col 0 = left). Do not mirror.
                    let px = cursor_x + (col as u32) * scale;
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

fn parse_hex_color(s: &str) -> Option<Rgba<u8>> {
    let s = s.trim();
    let hex = s.strip_prefix('#').unwrap_or(s);
    let bytes = match hex.len() {
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            [r, g, b, 0xff]
        }
        8 => {
            // Accept both AARRGGBB and RRGGBBAA. Prefer RRGGBBAA (common in CSS-like strings).
            let a_last = {
                let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
                let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
                let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
                let a = u8::from_str_radix(&hex[6..8], 16).ok()?;
                [r, g, b, a]
            };
            if a_last[3] == 0 && !hex.starts_with("00") {
                let a = u8::from_str_radix(&hex[0..2], 16).ok()?;
                let r = u8::from_str_radix(&hex[2..4], 16).ok()?;
                let g = u8::from_str_radix(&hex[4..6], 16).ok()?;
                let b = u8::from_str_radix(&hex[6..8], 16).ok()?;
                [r, g, b, a]
            } else {
                a_last
            }
        }
        _ => return None,
    };
    Some(Rgba(bytes))
}

/// Composite a small label onto an existing button image.
///
/// This is intentionally tiny/pixel-ish (8x8 font) to avoid depending on platform fonts.
/// The goal is consistency across hardware + preview rather than typographic perfection.
pub fn overlay_label(base: image::DynamicImage, overlay: &LabelOverlay) -> image::DynamicImage {
    let label = overlay.text.trim();
    if label.is_empty() {
        return base;
    }

    let mut img = base.to_rgba8();
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return image::DynamicImage::ImageRgba8(img);
    }

    // Auto-scale + wrap so text always fits.
    //
    // NOTE: This is tuned for perceived size on real Stream Deck hardware.
    // On 72x72 keys, scale=2 reads as "huge", so we keep scale=1 unless keys are larger.
    let min_side = w.min(h).max(1);
    // Slightly more conservative than the old thresholds: label size is now user-adjustable, and
    // we prefer icons to remain dominant by default.
    let max_scale = if min_side >= 144 {
        4
    } else if min_side >= 112 {
        3
    } else {
        1
    };

    // Convert Stream Deck "font size" into a scale multiplier for our 8x8 raster font.
    // This is intentionally a soft hint; we still clamp to fit the available label area.
    let mut size_factor = (overlay.size as f32) / 16.0;
    // Global tweak: make labels read slightly smaller by default (users can dial it back up).
    size_factor *= 0.92;
    size_factor = size_factor.clamp(0.5, 4.0);

    // Padding from the button edge.
    let pad = 4u32;

    // Available area to place text (no background strip; rely on a subtle shadow).
    let (max_w, max_h) = match overlay.placement {
        TextPlacement::Top | TextPlacement::Bottom => {
            // Reserve ~3/8 of the key for labels so icons still have room (matches preview better).
            let max_h = (h.saturating_mul(3) / 8).saturating_sub(2).max(10);
            (w.saturating_sub(pad * 2).max(1), max_h)
        }
        TextPlacement::Left | TextPlacement::Right => {
            // Side labels should be a narrow vertical column.
            let max_w = (w / 4).max(10).min(w.saturating_sub(pad * 2).max(1));
            (max_w, h.saturating_sub(pad * 2).max(1))
        }
        TextPlacement::Center => (
            w.saturating_sub(pad * 2).max(1),
            h.saturating_sub(pad * 2).max(1),
        ),
    };

    let ellipsize = |s: &str, max_chars: usize| -> String {
        if max_chars == 0 {
            return String::new();
        }
        let chars: Vec<char> = s.chars().collect();
        if chars.len() <= max_chars {
            return s.to_owned();
        }
        if max_chars <= 3 {
            return "...".chars().take(max_chars).collect();
        }
        let keep = max_chars.saturating_sub(3);
        chars.into_iter().take(keep).collect::<String>() + "..."
    };

    let wrap_words = |text: &str, max_cols: usize, max_lines: usize| -> Vec<String> {
        if max_cols == 0 || max_lines == 0 {
            return vec![];
        }
        let mut lines: Vec<String> = Vec::new();
        let mut current = String::new();

        let push_line = |line: String, lines: &mut Vec<String>| {
            if !line.trim().is_empty() {
                lines.push(line);
            }
        };

        // Split on whitespace, keep words (no punctuation awareness needed for this tiny font).
        for word in text.split_whitespace() {
            if lines.len() >= max_lines {
                break;
            }
            let sep = if current.is_empty() { "" } else { " " };
            let candidate = format!("{current}{sep}{word}");
            if candidate.chars().count() <= max_cols {
                current = candidate;
                continue;
            }

            // Commit current line if it has content.
            if !current.is_empty() {
                push_line(std::mem::take(&mut current), &mut lines);
                if lines.len() >= max_lines {
                    break;
                }
            }

            // Word longer than max_cols: hard-break.
            let mut remaining = word;
            while !remaining.is_empty() && lines.len() < max_lines {
                let chunk: String = remaining.chars().take(max_cols).collect();
                let taken = chunk.chars().count();
                push_line(chunk, &mut lines);
                remaining = &remaining[remaining
                    .char_indices()
                    .nth(taken)
                    .map(|(i, _)| i)
                    .unwrap_or(remaining.len())..];
            }
        }

        if lines.len() < max_lines && !current.is_empty() {
            push_line(current, &mut lines);
        }

        lines
    };

    // Special case: Left/Right should be truly vertical (stacked glyphs), not rotated sideways text.
    if matches!(
        overlay.placement,
        TextPlacement::Left | TextPlacement::Right
    ) {
        // Remove whitespace so "Hello World" doesn't become a mostly-empty vertical strip.
        let chars: Vec<char> = label.chars().filter(|c| !c.is_whitespace()).collect();
        if chars.is_empty() {
            return image::DynamicImage::ImageRgba8(img);
        }

        // Choose the largest scale (tuned earlier via `max_scale`) and compute how many
        // characters fit vertically.
        let scale = max_scale as u32;
        let line_step = 8 * scale + scale; // glyph height + spacing (unscaled)
        // Wrapping must reflect final (size-adjusted) pixel dimensions.
        let line_step_px = ((line_step as f32) * size_factor).round().max(1.0) as u32;
        let max_chars = (max_h / line_step_px).max(1) as usize;

        // Create "lines" where each line is a single character.
        let mut lines: Vec<String> = chars
            .into_iter()
            .take(max_chars)
            .map(|c| c.to_string())
            .collect();
        if lines.len() == max_chars
            && let Some(last) = lines.last_mut()
        {
            // If we truncated, show an ellipsis at the end.
            *last = "…".to_owned();
        }

        let text_w = (8 * scale).max(1);
        let text_h = ((lines.len() as u32) * (8 * scale)
            + (lines.len().saturating_sub(1) as u32) * scale)
            .max(1);

        let mut text_img = RgbaImage::new(text_w, text_h);
        let fg = parse_hex_color(&overlay.colour).unwrap_or(Rgba([255, 255, 255, 255]));
        for (i, line) in lines.iter().enumerate() {
            let y = (i as u32) * line_step;
            draw_text_8x8(&mut text_img, 1, y + 1, line, scale, Rgba([0, 0, 0, 200]));
            draw_text_8x8(&mut text_img, 0, y, line, scale, fg);
        }

        // Apply user scaling. If shrinking, we upscale first to preserve legibility.
        if (size_factor - 1.0).abs() > f32::EPSILON {
            let nw = ((text_img.width() as f32) * size_factor).round().max(1.0) as u32;
            let nh = ((text_img.height() as f32) * size_factor).round().max(1.0) as u32;
            text_img =
                image::imageops::resize(&text_img, nw, nh, image::imageops::FilterType::Triangle);
        }

        let x = match overlay.placement {
            TextPlacement::Left => pad,
            TextPlacement::Right => w.saturating_sub(text_img.width() + pad),
            _ => pad,
        };
        let y = (h.saturating_sub(text_img.height())) / 2;

        // Composite overlay.
        for yy in 0..text_img.height() {
            for xx in 0..text_img.width() {
                let dst_x = x + xx;
                let dst_y = y + yy;
                if dst_x < w && dst_y < h {
                    let src = *text_img.get_pixel(xx, yy);
                    if src[3] != 0 {
                        let dst = img.get_pixel_mut(dst_x, dst_y);
                        blend_pixel(dst, src);
                    }
                }
            }
        }

        return image::DynamicImage::ImageRgba8(img);
    }

    // Top/Bottom/Center: normal wrapped text.
    let mut chosen_scale = 1u32;
    let mut chosen_lines: Vec<String> = vec![label.to_owned()];

    for scale in (1..=max_scale).rev() {
        let scale = scale as u32;
        // Wrapping must reflect final (size-adjusted) pixel dimensions.
        let char_w = 8 * scale + scale;
        let line_h = 8 * scale;
        let char_w_px = (char_w as f32) * size_factor;
        let line_h_px = (line_h as f32) * size_factor;
        let gap_px = (scale as f32) * size_factor;

        // Prefer more lines when centered (it intentionally overlays the icon), but keep
        // Top/Bottom conservative so icons remain dominant.
        let preferred_lines: usize = if overlay.placement == TextPlacement::Center {
            3
        } else {
            2
        };
        let mut max_lines = 1usize;
        for lines in (1usize..=preferred_lines).rev() {
            let needed_h = (lines as f32) * line_h_px + ((lines.saturating_sub(1)) as f32) * gap_px;
            if (max_h as f32) >= needed_h {
                max_lines = lines;
                break;
            }
        }
        let max_cols = ((max_w as f32) / char_w_px).floor().max(1.0) as usize;

        let mut lines = wrap_words(label, max_cols, max_lines);
        if lines.is_empty() {
            lines = vec![String::new()];
        }

        // Ellipsize if we had to truncate lines.
        if lines.len() == max_lines {
            let last = lines.last().cloned().unwrap_or_default();
            let last = ellipsize(&last, max_cols);
            if let Some(last_mut) = lines.last_mut() {
                *last_mut = last;
            }
        }

        let max_line_chars = lines
            .iter()
            .map(|l| l.chars().count() as u32)
            .max()
            .unwrap_or(0);
        // Predict final size after applying `size_factor` so we choose wrapping correctly.
        let text_w_px = (max_line_chars as f32) * char_w_px;
        let text_h_px =
            (lines.len() as f32) * line_h_px + ((lines.len().saturating_sub(1)) as f32) * gap_px;

        if text_w_px <= (max_w as f32) && text_h_px <= (max_h as f32) {
            chosen_scale = scale;
            chosen_lines = lines;
            break;
        }
    }

    let scale = chosen_scale;
    let char_w = 8 * scale + scale;
    let line_step = 8 * scale + scale;
    let max_line_chars = chosen_lines
        .iter()
        .map(|l| l.chars().count() as u32)
        .max()
        .unwrap_or(0);
    let text_w = (max_line_chars * char_w).max(1);
    let text_h = ((chosen_lines.len() as u32) * (8 * scale)
        + (chosen_lines.len().saturating_sub(1) as u32) * scale)
        .max(1);

    let mut text_img = RgbaImage::new(text_w, text_h);
    let fg = parse_hex_color(&overlay.colour).unwrap_or(Rgba([255, 255, 255, 255]));
    for (i, line) in chosen_lines.iter().enumerate() {
        let y = (i as u32) * line_step;
        draw_text_8x8(&mut text_img, 1, y + 1, line, scale, Rgba([0, 0, 0, 200]));
        draw_text_8x8(&mut text_img, 0, y, line, scale, fg);
    }

    if (size_factor - 1.0).abs() > f32::EPSILON {
        let nw = ((text_img.width() as f32) * size_factor).round().max(1.0) as u32;
        let nh = ((text_img.height() as f32) * size_factor).round().max(1.0) as u32;
        text_img =
            image::imageops::resize(&text_img, nw, nh, image::imageops::FilterType::Triangle);
    }

    let x = (w.saturating_sub(text_img.width())) / 2;
    let y = match overlay.placement {
        TextPlacement::Top => pad.saturating_sub(2),
        TextPlacement::Bottom => h.saturating_sub(text_img.height() + pad.saturating_sub(2)),
        TextPlacement::Center => (h.saturating_sub(text_img.height())) / 2,
        _ => pad,
    };

    // Composite overlay.
    for yy in 0..text_img.height() {
        for xx in 0..text_img.width() {
            let dst_x = x + xx;
            let dst_y = y + yy;
            if dst_x < w && dst_y < h {
                let src = *text_img.get_pixel(xx, yy);
                if src[3] != 0 {
                    let dst = img.get_pixel_mut(dst_x, dst_y);
                    blend_pixel(dst, src);
                }
            }
        }
    }

    image::DynamicImage::ImageRgba8(img)
}
