use std::collections::HashMap;

use crate::shared::{Action, ActionState};

use serde::Deserialize;
use serde_inline_default::serde_inline_default;

#[derive(Debug, Deserialize)]
pub struct OS {
    #[serde(alias = "Platform")]
    pub platform: String,
}

#[allow(dead_code)]
#[serde_inline_default]
#[derive(Deserialize)]
pub struct PluginManifest {
    #[serde(alias = "Name")]
    pub name: String,

    #[serde(alias = "Author")]
    pub author: String,

    #[serde(alias = "Version")]
    pub version: String,

    #[serde(alias = "Icon")]
    pub icon: String,

    #[serde_inline_default("Custom".to_owned())]
    #[serde(alias = "Category")]
    pub category: String,

    #[serde(alias = "CategoryIcon")]
    pub category_icon: Option<String>,

    #[serde(alias = "Actions")]
    pub actions: Vec<Action>,

    #[serde(alias = "OS")]
    pub os: Vec<OS>,

    #[serde(alias = "CodePath")]
    pub code_path: Option<String>,

    #[serde(alias = "CodePaths")]
    pub code_paths: Option<HashMap<String, String>>,

    #[serde(alias = "CodePathWin")]
    pub code_path_windows: Option<String>,

    #[serde(alias = "CodePathMac")]
    pub code_path_macos: Option<String>,

    #[serde(alias = "CodePathLin")]
    pub code_path_linux: Option<String>,

    #[serde(alias = "PropertyInspectorPath")]
    pub property_inspector_path: Option<String>,

    #[serde(alias = "DeviceNamespace")]
    pub device_namespace: Option<String>,

    #[serde(alias = "ApplicationsToMonitor")]
    pub applications_to_monitor: Option<HashMap<String, Vec<String>>>,

    #[serde(alias = "HasSettingsInterface")]
    pub has_settings_interface: Option<bool>,
}

pub fn read_manifest(base_path: &std::path::Path) -> Result<PluginManifest, anyhow::Error> {
    use anyhow::Context;

    fn decode_manifest(bytes: &[u8]) -> anyhow::Result<String> {
        // Some Marketplace plugins ship a proprietary binary "manifest.json" container.
        // (Observed header: "ELGATO"). We can't decode this format yet, but we can synthesize
        // a manifest from the plugin's metadata files (localization, icon paths, etc).
        if bytes.starts_with(b"ELGATO") {
            return Err(anyhow::anyhow!(
                "manifest is in Elgato-protected binary format (ELGATO header), not JSON text"
            ));
        }

        // 1) UTF-8 BOM
        if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
            return String::from_utf8(bytes[3..].to_vec())
                .context("manifest is not valid UTF-8 (after UTF-8 BOM)");
        }

        // 2) UTF-16 BOM
        let decode_utf16 = |le: bool, data: &[u8]| -> anyhow::Result<String> {
            // Some manifests have a trailing NUL byte. Tolerate it.
            let data = if !data.len().is_multiple_of(2) {
                if data.last() == Some(&0) {
                    &data[..data.len() - 1]
                } else {
                    return Err(anyhow::anyhow!("manifest UTF-16 payload has odd length"));
                }
            } else {
                data
            };

            let mut u16s = Vec::with_capacity(data.len() / 2);
            for chunk in data.chunks_exact(2) {
                let v = if le {
                    u16::from_le_bytes([chunk[0], chunk[1]])
                } else {
                    u16::from_be_bytes([chunk[0], chunk[1]])
                };
                u16s.push(v);
            }
            String::from_utf16(&u16s).context("manifest is not valid UTF-16")
        };

        // 2.5) UTF-32 BOM
        let decode_utf32 = |le: bool, data: &[u8]| -> anyhow::Result<String> {
            // Tolerate up to 3 trailing NUL bytes.
            let mut data = data;
            while !data.len().is_multiple_of(4) && data.last() == Some(&0) {
                data = &data[..data.len() - 1];
            }
            if !data.len().is_multiple_of(4) {
                return Err(anyhow::anyhow!(
                    "manifest UTF-32 payload has invalid length"
                ));
            }
            let mut out = String::with_capacity(data.len() / 2);
            for chunk in data.chunks_exact(4) {
                let cp = if le {
                    u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
                } else {
                    u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
                };
                let Some(ch) = char::from_u32(cp) else {
                    return Err(anyhow::anyhow!(
                        "manifest UTF-32 contained invalid codepoint"
                    ));
                };
                out.push(ch);
            }
            Ok(out)
        };

        if bytes.starts_with(&[0xFF, 0xFE]) {
            return decode_utf16(true, &bytes[2..]);
        }
        if bytes.starts_with(&[0xFE, 0xFF]) {
            return decode_utf16(false, &bytes[2..]);
        }
        if bytes.starts_with(&[0xFF, 0xFE, 0x00, 0x00]) {
            return decode_utf32(true, &bytes[4..]);
        }
        if bytes.starts_with(&[0x00, 0x00, 0xFE, 0xFF]) {
            return decode_utf32(false, &bytes[4..]);
        }

        // 3) Try plain UTF-8.
        if let Ok(s) = std::str::from_utf8(bytes) {
            return Ok(s.to_owned());
        }

        // 4) Heuristic: if it looks like UTF-16LE/BE or UTF-32LE/BE, try decoding.
        let head = &bytes[..bytes.len().min(256)];
        let nul_count = head.iter().filter(|&&b| b == 0).count();
        let looks_utf16ish = nul_count >= 2
            || (bytes.len() >= 2 && bytes[0] == b'{' && bytes[1] == 0)
            || (bytes.len() >= 2 && bytes[0] == 0 && bytes[1] == b'{');
        let looks_utf32ish = nul_count >= 8
            || (bytes.len() >= 4
                && bytes[0] == b'{'
                && bytes[1] == 0
                && bytes[2] == 0
                && bytes[3] == 0)
            || (bytes.len() >= 4
                && bytes[0] == 0
                && bytes[1] == 0
                && bytes[2] == 0
                && bytes[3] == b'{');

        if looks_utf32ish {
            // Guess LE/BE and try both.
            let le_guess =
                bytes.get(1) == Some(&0) && bytes.get(2) == Some(&0) && bytes.get(3) == Some(&0);
            if let Ok(s) = decode_utf32(le_guess, bytes) {
                return Ok(s);
            }
            if let Ok(s) = decode_utf32(!le_guess, bytes) {
                return Ok(s);
            }
        }

        if looks_utf16ish {
            // Prefer LE/BE based on where the NULs show up for ASCII-ish JSON, but try both.
            let le_guess = bytes.get(1) == Some(&0);
            if let Ok(s) = decode_utf16(le_guess, bytes) {
                return Ok(s);
            }
            if let Ok(s) = decode_utf16(!le_guess, bytes) {
                return Ok(s);
            }
        }

        Err(anyhow::anyhow!(
            "manifest encoding unsupported (not UTF-8/UTF-16/UTF-32)"
        ))
    }

    let raw = std::fs::read(base_path.join("manifest.json")).context("failed to read manifest")?;

    // Try to decode the manifest; if it's ELGATO format, synthesize from metadata instead.
    let manifest = match decode_manifest(&raw) {
        Ok(text) => {
            let text = text.trim_start_matches("\u{feff}");
            let mut manifest: serde_json::Value =
                serde_json::from_str(text).context("failed to parse manifest")?;

            let platform_overrides_path =
                base_path.join(format!("manifest.{}.json", std::env::consts::OS));
            if platform_overrides_path.exists()
                && let Ok(Ok(platform_overrides)) =
                    std::fs::read(platform_overrides_path).map(|v| serde_json::from_slice(&v))
            {
                json_patch::merge(&mut manifest, &platform_overrides);
            }

            serde_json::from_value(manifest).context("failed to parse manifest")?
        }
        Err(e) if e.to_string().contains("ELGATO") => {
            // ELGATO binary format detected - synthesize manifest from plugin metadata
            log::info!(
                "ELGATO manifest format detected at {}, synthesizing from metadata",
                base_path.display()
            );
            synthesize_manifest_from_bundle(base_path)?
        }
        Err(e) => return Err(e),
    };

    Ok(manifest)
}

/// Synthesize a manifest from plugin bundle metadata (for ELGATO-format plugins).
///
/// This scans the plugin folder for localization files (`en.json`, etc), icons, and
/// property inspectors to construct a valid manifest without decoding the binary container.
fn synthesize_manifest_from_bundle(
    base_path: &std::path::Path,
) -> Result<PluginManifest, anyhow::Error> {
    // Read localization file to extract action names/descriptions
    let en_json_path = base_path.join("en.json");
    let localization: serde_json::Value = if en_json_path.exists() {
        serde_json::from_slice(&std::fs::read(&en_json_path)?)?
    } else {
        serde_json::json!({})
    };

    // Infer plugin metadata from folder structure and localization
    let plugin_id = base_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");

    let plugin_name = localization
        .get("Name")
        .or_else(|| localization.get("PluginName"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
        .unwrap_or_else(|| {
            // Infer from plugin ID: "com.elgato.volume-controller.sdPlugin" -> "Volume Controller"
            plugin_id
                .strip_suffix(".sdPlugin")
                .unwrap_or(plugin_id)
                .split('.')
                .next_back()
                .unwrap_or("Plugin")
                .split('-')
                .map(|s| {
                    let mut c = s.chars();
                    match c.next() {
                        None => String::new(),
                        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        });

    let _description = localization
        .get("Description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();

    // Find code path (look for bin/plugin.js, plugin.js, or other common entrypoints)
    let code_path = if base_path.join("bin/plugin.js").exists() {
        Some("bin/plugin.js".to_owned())
    } else if base_path.join("plugin.js").exists() {
        Some("plugin.js".to_owned())
    } else if base_path.join("bin/plugin.cjs").exists() {
        Some("bin/plugin.cjs".to_owned())
    } else if base_path.join("plugin.cjs").exists() {
        Some("plugin.cjs".to_owned())
    } else if base_path.join("bin/plugin.exe").exists() {
        Some("bin/plugin.exe".to_owned())
    } else if base_path.join("plugin.exe").exists() {
        Some("plugin.exe".to_owned())
    } else if base_path.join("bin/index.js").exists() {
        Some("bin/index.js".to_owned())
    } else if base_path.join("index.js").exists() {
        Some("index.js".to_owned())
    } else if base_path.join("plugin.html").exists() {
        Some("plugin.html".to_owned())
    } else {
        // Look for ESD* pattern executables (e.g., ESDDiscord, ESDDiscord.exe)
        // These are native Elgato plugin executables
        // Collect all ESD* executables, then choose the best one based on platform
        let mut found_exes: Vec<String> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(base_path) {
            for entry in entries.flatten() {
                let file_name = entry.file_name();
                let name_str = file_name.to_string_lossy();
                if name_str.starts_with("ESD")
                    && (name_str.ends_with(".exe") || !name_str.contains('.')) {
                        found_exes.push(name_str.to_string());
                    }
            }
        }

        if !found_exes.is_empty() {
            // Try to find platform-specific executable
            // Priority: native executable for current platform, then .exe (for Wine)
            #[cfg(target_os = "linux")]
            let preferred = found_exes
                .iter()
                .find(|e| !e.ends_with(".exe"))
                .and_then(|e| {
                    // Check if it's actually a Linux ELF binary
                    let exe_path = base_path.join(e);
                    let file_output = std::process::Command::new("file")
                        .arg(&exe_path)
                        .output()
                        .ok()
                        .and_then(|o| String::from_utf8(o.stdout).ok())
                        .unwrap_or_default()
                        .to_lowercase();
                    if file_output.contains("elf") {
                        Some(e.clone())
                    } else {
                        None
                    }
                });

            #[cfg(target_os = "macos")]
            let preferred = found_exes
                .iter()
                .find(|e| !e.ends_with(".exe"))
                .and_then(|e| {
                    // Check if it's actually a macOS Mach-O binary
                    let exe_path = base_path.join(e);
                    let file_output = std::process::Command::new("file")
                        .arg(&exe_path)
                        .output()
                        .ok()
                        .and_then(|o| String::from_utf8(o.stdout).ok())
                        .unwrap_or_default()
                        .to_lowercase();
                    if file_output.contains("mach-o") {
                        Some(e.clone())
                    } else {
                        None
                    }
                });

            #[cfg(target_os = "windows")]
            let preferred = found_exes
                .iter()
                .find(|e| e.ends_with(".exe"))
                .map(|e| e.clone());

            // Fall back to .exe for Wine if no native binary found
            let found_exe = preferred.or_else(|| {
                found_exes
                    .iter()
                    .find(|e| e.ends_with(".exe")).cloned()
            });

            if found_exe.is_some() {
                found_exe
            } else {
                log::warn!(
                    "Found ESD executables at {} but none are suitable for this platform",
                    base_path.display()
                );
                None
            }
        } else {
            log::warn!(
                "No code path found for ELGATO plugin at {}",
                base_path.display()
            );
            None
        }
    };

    // Find plugin icon
    let icon = if base_path.join("imgs/plugin/marketplace.png").exists() {
        "imgs/plugin/marketplace.png".to_owned()
    } else if base_path.join("images/plugin.png").exists() {
        "images/plugin.png".to_owned()
    } else if base_path.join("icon.png").exists() {
        "icon.png".to_owned()
    } else {
        String::new()
    };

    let category_icon = if base_path.join("imgs/plugin/category-icon.svg").exists() {
        Some("imgs/plugin/category-icon.svg".to_owned())
    } else if base_path.join("images/category.svg").exists() {
        Some("images/category.svg".to_owned())
    } else {
        None
    };

    // Extract actions from localization file
    let mut actions = Vec::new();

    if let Some(obj) = localization.as_object() {
        for (key, value) in obj {
            // Skip non-action entries (Description, Localization, etc)
            if key == "Description" || key == "Localization" || key == "Name" || key == "PluginName"
            {
                continue;
            }

            // Action UUIDs typically match keys in localization (e.g., "com.elgato.volume-controller.auto-detection")
            if key.contains('.') {
                let uuid = key.clone();
                let action_name = value
                    .get("Name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(key)
                    .to_owned();

                let tooltip = value
                    .get("Tooltip")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();

                // Determine controllers (Keypad, Encoder, or both)
                let controllers = if value.get("Encoder").is_some() {
                    vec!["Keypad".to_owned(), "Encoder".to_owned()]
                } else {
                    vec!["Keypad".to_owned()]
                };

                // Find icon for this action by trying multiple slug extraction strategies
                // and checking which folders actually exist
                let possible_slugs = {
                    let mut slugs = Vec::new();
                    let segments: Vec<&str> = uuid.split('.').collect();

                    // Strategy 1: Last segment (e.g., "auto-detection")
                    if let Some(last) = segments.last() {
                        slugs.push((*last).to_owned());
                    }

                    // Strategy 2: For "auto-detection.X" pattern, try "auto-X"
                    // (common pattern in Volume Controller plugin)
                    if segments.len() >= 2 && segments[segments.len() - 2] == "auto-detection" {
                        slugs.push(format!("auto-{}", segments[segments.len() - 1]));
                    }

                    // Strategy 3: Try prepending "auto-" to last segment
                    // (e.g., "back-to-profile" -> "auto-back-to-profile")
                    if let Some(last) = segments.last() {
                        slugs.push(format!("auto-{}", last));
                    }

                    // Strategy 4: Last two segments with dash (e.g., "auto-detection-volume")
                    if segments.len() >= 2 {
                        slugs.push(format!(
                            "{}-{}",
                            segments[segments.len() - 2],
                            segments[segments.len() - 1]
                        ));
                    }

                    // Strategy 5: Convert action name to slug (kebab-case)
                    // e.g., "Auto Volume" -> "auto-volume"
                    let name_slug = action_name.to_lowercase().replace([' ', '/'], "-");
                    slugs.push(name_slug);

                    slugs
                };

                // Try each slug and find the first matching icon/PI
                let mut action_icon = String::new();
                let mut state_icon = String::new();
                let mut property_inspector = String::new();

                log::debug!(
                    "Searching for icons for action '{}' (uuid: {}), trying slugs: {:?}",
                    action_name,
                    uuid,
                    possible_slugs
                );

                for slug in &possible_slugs {
                    if action_icon.is_empty() {
                        // Prefer PNG/JPG, but fall back to SVG (will be converted at runtime)
                        let candidates = [
                            format!("imgs/actions/{}/action.png", slug),
                            format!("imgs/actions/{}/action@2x.png", slug),
                            format!("imgs/actions/{}/key.png", slug),
                            format!("imgs/actions/{}/key@2x.png", slug),
                            format!("imgs/actions/{}/action.jpg", slug),
                            format!("imgs/actions/{}/key.jpg", slug),
                            format!("imgs/actions/{}/action.svg", slug),
                            format!("imgs/actions/{}/key.svg", slug),
                            format!("images/actions/{}.svg", slug),
                            format!("images/actions/{}.png", slug),
                            format!("images/actions/{}_0.svg", slug),
                            format!("images/actions/{}_0.png", slug),
                        ];
                        action_icon = candidates
                            .iter()
                            .find(|p| {
                                let full_path = base_path.join(p);
                                if !full_path.exists() {
                                    return false;
                                }
                                // Skip 1-bit monochrome placeholders (< 300 bytes, likely blank)
                                // These are common in Elgato plugins that draw icons dynamically
                                if let Ok(metadata) = std::fs::metadata(&full_path)
                                    && metadata.len() < 300
                                    && p.ends_with(".png")
                                {
                                    // Check if it's 1-bit by trying to read it
                                    if let Ok(img) = image::open(&full_path) {
                                        // 1-bit images show as Luma8, tiny files are likely blank
                                        if img.color().channel_count() == 1 && metadata.len() < 300
                                        {
                                            return false;
                                        }
                                    }
                                }
                                true
                            })
                            .cloned()
                            .unwrap_or_default();

                        // If no valid icon found, try category icon as fallback
                        if action_icon.is_empty() {
                            let category_candidates = [
                                format!("imgs/actions/{}/category.png", slug),
                                format!("imgs/actions/{}/category@2x.png", slug),
                            ];
                            action_icon = category_candidates
                                .iter()
                                .find(|p| base_path.join(p).exists())
                                .cloned()
                                .unwrap_or_default();
                        }
                    }

                    if state_icon.is_empty() {
                        // Prefer PNG/JPG, but fall back to SVG (will be converted at runtime)
                        let state_candidates = [
                            format!("imgs/actions/{}/key.png", slug),
                            format!("imgs/actions/{}/key@2x.png", slug),
                            format!("imgs/actions/{}/action.png", slug),
                            format!("imgs/actions/{}/action@2x.png", slug),
                            format!("imgs/actions/{}/key.jpg", slug),
                            format!("imgs/actions/{}/action.jpg", slug),
                            format!("imgs/actions/{}/key.svg", slug),
                            format!("imgs/actions/{}/action.svg", slug),
                            format!("images/actions/{}.svg", slug),
                            format!("images/actions/{}.png", slug),
                            format!("images/actions/{}_0.svg", slug),
                            format!("images/actions/{}_0.png", slug),
                        ];
                        state_icon = state_candidates
                            .iter()
                            .find(|p| {
                                let full_path = base_path.join(p);
                                if !full_path.exists() {
                                    return false;
                                }
                                // Skip 1-bit monochrome placeholders (< 300 bytes, likely blank)
                                if let Ok(metadata) = std::fs::metadata(&full_path)
                                    && metadata.len() < 300
                                    && p.ends_with(".png")
                                {
                                    // Check if it's 1-bit by trying to read it
                                    if let Ok(img) = image::open(&full_path) {
                                        // 1-bit images show as Luma8, tiny files are likely blank
                                        if img.color().channel_count() == 1 && metadata.len() < 300
                                        {
                                            return false;
                                        }
                                    }
                                }
                                true
                            })
                            .cloned()
                            .unwrap_or_default();

                        // If no valid state icon found, try category icon as fallback
                        if state_icon.is_empty() {
                            let category_candidates = [
                                format!("imgs/actions/{}/category.png", slug),
                                format!("imgs/actions/{}/category@2x.png", slug),
                                "images/category.png".to_string(),
                                "images/category@2x.png".to_string(),
                            ];
                            state_icon = category_candidates
                                .iter()
                                .find(|p| base_path.join(p).exists())
                                .cloned()
                                .unwrap_or_default();
                        }
                    }

                    if property_inspector.is_empty() {
                        let pi_candidates = [
                            format!("pi/application/{}/inspector.html", slug),
                            format!("pi/device/{}/inspector.html", slug),
                            format!("pi/{}/inspector.html", slug),
                        ];
                        property_inspector = pi_candidates
                            .iter()
                            .find(|p| base_path.join(p).exists())
                            .cloned()
                            .unwrap_or_default();
                    }

                    // If we found everything, we're done
                    if !action_icon.is_empty()
                        && !state_icon.is_empty()
                        && !property_inspector.is_empty()
                    {
                        break;
                    }
                }

                // Final fallback to blank.png if nothing found
                if action_icon.is_empty() && base_path.join("blank.png").exists() {
                    action_icon = "blank.png".to_owned();
                }
                if state_icon.is_empty() && base_path.join("blank.png").exists() {
                    state_icon = "blank.png".to_owned();
                }

                log::debug!(
                    "Action '{}': icon='{}', state_icon='{}', PI='{}'",
                    action_name,
                    action_icon,
                    state_icon,
                    property_inspector
                );

                actions.push(Action {
                    name: action_name,
                    uuid: uuid.clone(),
                    plugin: String::new(), // Will be filled by caller
                    tooltip,
                    icon: action_icon,
                    disable_automatic_states: false,
                    visible_in_action_list: true,
                    supported_in_multi_actions: true,
                    property_inspector,
                    controllers,
                    states: vec![ActionState {
                        image: state_icon,
                        ..Default::default()
                    }],
                });
            }
        }
    }

    // Determine OS support based on the code path type
    let (os, code_path_windows, code_path_macos, code_path_linux) = if let Some(ref cp) = code_path
    {
        if cp.ends_with(".js")
            || cp.ends_with(".cjs")
            || cp.ends_with(".mjs")
            || cp.ends_with(".html")
        {
            // Node.js and HTML plugins are cross-platform
            (
                vec![
                    OS {
                        platform: "windows".to_owned(),
                    },
                    OS {
                        platform: "mac".to_owned(),
                    },
                    OS {
                        platform: "linux".to_owned(),
                    },
                ],
                code_path.clone(),
                code_path.clone(),
                code_path.clone(),
            )
        } else if cp.ends_with(".exe") {
            // Windows executable - use Wine on non-Windows
            (
                vec![OS {
                    platform: "windows".to_owned(),
                }],
                code_path.clone(),
                None,
                None,
            )
        } else {
            // Native executable without extension - need to detect platform
            // Check the actual file to determine its type
            let exe_path = base_path.join(cp);
            let file_output = std::process::Command::new("file")
                .arg(&exe_path)
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .unwrap_or_default()
                .to_lowercase();

            log::debug!("Detected executable type for {}: {}", cp, file_output);

            if file_output.contains("mach-o") {
                // macOS binary
                (
                    vec![OS {
                        platform: "mac".to_owned(),
                    }],
                    None,
                    code_path.clone(),
                    None,
                )
            } else if file_output.contains("elf") {
                // Linux binary
                (
                    vec![OS {
                        platform: "linux".to_owned(),
                    }],
                    None,
                    None,
                    code_path.clone(),
                )
            } else if file_output.contains("pe32") || file_output.contains("pe executable") {
                // Windows binary without .exe extension
                (
                    vec![OS {
                        platform: "windows".to_owned(),
                    }],
                    code_path.clone(),
                    None,
                    None,
                )
            } else {
                // Unknown format, assume Windows and try Wine
                log::warn!(
                    "Could not determine executable format for {}, assuming Windows",
                    cp
                );
                (
                    vec![OS {
                        platform: "windows".to_owned(),
                    }],
                    code_path.clone(),
                    None,
                    None,
                )
            }
        }
    } else {
        // No code path found
        (vec![], None, None, None)
    };

    let manifest = PluginManifest {
        name: plugin_name,
        author: "Elgato".to_owned(), // Could be inferred from plugin ID namespace
        version: "1.0.0".to_owned(), // Could try to parse from filename or assume default
        icon,
        category: "Audio".to_owned(), // Could be made configurable or inferred
        category_icon,
        actions,
        os,
        code_path: code_path.clone(),
        code_paths: None,
        code_path_windows,
        code_path_macos,
        code_path_linux,
        property_inspector_path: None,
        device_namespace: None,
        applications_to_monitor: None,
        has_settings_interface: None,
    };

    log::debug!(
        "Synthesized ELGATO manifest: os={:?}, code_path={:?}, code_path_windows={:?}, code_path_linux={:?}",
        manifest.os,
        manifest.code_path,
        manifest.code_path_windows,
        manifest.code_path_linux
    );

    Ok(manifest)
}
