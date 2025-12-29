use std::collections::HashMap;
use std::env::var;
use std::path::{Path, PathBuf};

use directories::BaseDirs;
use log::warn;
use serde::{Deserialize, Deserializer, Serialize, de::Visitor};
use serde_inline_default::serde_inline_default;

use dashmap::DashMap;
use once_cell::sync::{Lazy, OnceCell};
use tokio::sync::RwLock;

pub const PRODUCT_NAME: &str = include_str!("../../../product_name.txt").trim_ascii();

// Built-in actions (canonical RiverDeck identifiers).
pub const BUILTIN_PLUGIN_ID: &str = "riverdeck";
pub const MULTI_ACTION_UUID: &str = "riverdeck.multiaction";
pub const TOGGLE_ACTION_UUID: &str = "riverdeck.toggleaction";

// Built-in plugin IDs / UUID namespaces that we want to keep stable, even if older versions used
// upstream identifiers.
pub const STARTERPACK_PLUGIN_ID: &str = "io.github.sulrwin.riverdeck.starterpack.sdPlugin";
pub const LEGACY_STARTERPACK_PLUGIN_ID: &str = "com.amansprojects.starterpack.sdPlugin";
pub const STARTERPACK_UUID_PREFIX: &str = "io.github.sulrwin.riverdeck.starterpack.";
pub const LEGACY_STARTERPACK_UUID_PREFIX: &str = "com.amansprojects.starterpack.";

pub fn is_multi_action_uuid(uuid: &str) -> bool {
    uuid == MULTI_ACTION_UUID || uuid == "opendeck.multiaction"
}

pub fn is_toggle_action_uuid(uuid: &str) -> bool {
    uuid == TOGGLE_ACTION_UUID || uuid == "opendeck.toggleaction"
}

pub fn normalize_builtin_action(plugin: &mut String, uuid: &mut String) {
    // Treat legacy OpenDeck built-in actions as RiverDeck built-ins.
    if plugin.as_str() == "opendeck" {
        *plugin = BUILTIN_PLUGIN_ID.to_owned();
    }
    match uuid.as_str() {
        "opendeck.multiaction" => *uuid = MULTI_ACTION_UUID.to_owned(),
        "opendeck.toggleaction" => *uuid = TOGGLE_ACTION_UUID.to_owned(),
        _ => {}
    }
}

/// Normalize legacy starter pack identifiers to canonical RiverDeck identifiers.
///
/// This keeps old profiles/settings working when the bundled starter pack plugin was renamed from
/// `com.amansprojects.starterpack` to `io.github.sulrwin.riverdeck.starterpack`.
pub fn normalize_starterpack_action(plugin: &mut String, uuid: &mut String) {
    if plugin.as_str() == LEGACY_STARTERPACK_PLUGIN_ID {
        *plugin = STARTERPACK_PLUGIN_ID.to_owned();
    }
    if let Some(rest) = uuid.strip_prefix(LEGACY_STARTERPACK_UUID_PREFIX) {
        *uuid = format!("{STARTERPACK_UUID_PREFIX}{rest}");
    }
}

/// Rewrite legacy starter pack paths (absolute or relative) to the canonical plugin folder.
///
/// This primarily matters for older profiles that stored absolute icon/PI paths under
/// `.../plugins/com.amansprojects.starterpack.sdPlugin/...`.
pub fn normalize_starterpack_paths(value: &mut String) {
    // Handle both unix-style and windows-style separators. We only touch the plugin segment.
    *value = value.replace(
        &format!("/plugins/{LEGACY_STARTERPACK_PLUGIN_ID}/"),
        &format!("/plugins/{STARTERPACK_PLUGIN_ID}/"),
    );
    *value = value.replace(
        &format!("\\plugins\\{LEGACY_STARTERPACK_PLUGIN_ID}\\"),
        &format!("\\plugins\\{STARTERPACK_PLUGIN_ID}\\"),
    );
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub log_dir: PathBuf,
    /// Optional directory containing bundled resources (e.g. builtin plugins).
    pub resource_dir: Option<PathBuf>,
}

static PATHS: OnceCell<Paths> = OnceCell::new();

pub fn init_paths(paths: Paths) {
    let _ = PATHS.set(paths);
}

pub fn discover_paths() -> anyhow::Result<Paths> {
    let base =
        BaseDirs::new().ok_or_else(|| anyhow::anyhow!("failed to determine base directories"))?;
    let app_id = "io.github.sulrwin.riverdeck";

    let config_dir = base.config_dir().join(app_id);
    let data_dir = base.data_dir().join(app_id);
    let log_dir = data_dir.join("logs");

    Ok(Paths {
        config_dir,
        data_dir,
        log_dir,
        resource_dir: None,
    })
}

fn paths() -> &'static Paths {
    PATHS
        .get()
        .expect("riverdeck-core paths not initialised; call shared::init_paths() early in main()")
}

pub fn copy_dir(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> Result<(), std::io::Error> {
    use std::fs;
    fs::create_dir_all(&dst)?;
    for entry in fs::read_dir(src)?.flatten() {
        if entry.file_type()?.is_dir() {
            copy_dir(entry.path(), dst.as_ref().join(entry.file_name()))?;
        } else {
            fs::copy(entry.path(), dst.as_ref().join(entry.file_name()))?;
        }
    }
    Ok(())
}

/// Metadata of a device.
#[serde_inline_default]
#[derive(Clone, Deserialize, Serialize)]
pub struct DeviceInfo {
    pub id: String,
    #[serde_inline_default(String::new())]
    pub plugin: String,
    pub name: String,
    pub rows: u8,
    pub columns: u8,
    pub encoders: u8,
    pub r#type: u8,
    #[serde(default)]
    pub screen: Option<DeviceScreenInfo>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ScreenPlacement {
    BetweenKeypadAndEncoders,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceScreenInfo {
    pub width_px: u16,
    pub height_px: u16,
    pub segments: u8,
    pub placement: ScreenPlacement,
}

pub static DEVICES: Lazy<DashMap<String, DeviceInfo>> = Lazy::new(DashMap::new);

/// Get the application configuration directory.
pub fn config_dir() -> std::path::PathBuf {
    paths().config_dir.clone()
}

/// Get the application log directory.
pub fn log_dir() -> std::path::PathBuf {
    paths().log_dir.clone()
}

/// Get the application data directory.
pub fn data_dir() -> std::path::PathBuf {
    paths().data_dir.clone()
}

pub fn resource_dir() -> Option<PathBuf> {
    paths().resource_dir.clone()
}

/// Ensure core built-in action icons exist on disk.
///
/// Core built-ins reference `riverdeck/*.png` paths. In packaged builds these typically live under
/// `resource_dir`, but in dev/debug builds we may not have bundled resources. To keep built-in
/// actions functional (and avoid noisy "image path not found" warnings), create placeholder icons
/// under `config_dir/riverdeck/` if they are missing.
pub fn ensure_builtin_icons() {
    let dir = config_dir().join("riverdeck");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }

    const MULTI_SVG: &[u8] = include_bytes!("../assets/riverdeck/multi-action.svg");
    const TOGGLE_SVG: &[u8] = include_bytes!("../assets/riverdeck/toggle-action.svg");

    fn ensure_file(path: &Path, bytes: &[u8]) {
        if path.is_file() {
            return;
        }
        if let Err(err) = std::fs::write(path, bytes) {
            warn!("Failed to write builtin icon {}: {}", path.display(), err);
        }
    }

    fn looks_like_old_placeholder_png(path: &Path, rgb: [u8; 3]) -> bool {
        let Ok(img) = image::open(path) else {
            return false;
        };
        let rgba = img.to_rgba8();
        let Some(first) = rgba.pixels().next() else {
            return false;
        };
        // Fully opaque solid fill in the specific placeholder color.
        if first.0[0..3] != rgb || first.0[3] != 0xff {
            return false;
        }
        rgba.pixels().all(|p| p.0 == first.0)
    }

    fn ensure_png_from_svg(path: &Path, svg_bytes: &[u8]) {
        // Preserve user overrides, but automatically replace our old solid-color placeholders.
        if path.is_file()
            && !looks_like_old_placeholder_png(
                path,
                // Multi/Toggle placeholder colors from earlier versions.
                if path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .contains("multi-action")
                {
                    [0x2d, 0x9c, 0xdb]
                } else {
                    [0xf2, 0xc9, 0x4c]
                },
            )
        {
            return;
        }

        let img = match crate::elgato::convert_svg_to_image(svg_bytes) {
            Ok(img) => img,
            Err(err) => {
                warn!(
                    "Failed to rasterize builtin SVG {}: {}",
                    path.display(),
                    err
                );
                return;
            }
        };
        if let Err(err) = img.save_with_format(path, image::ImageFormat::Png) {
            warn!("Failed to create builtin icon {}: {}", path.display(), err);
        }
    }

    // Install both SVG (for UI/icon picker) and PNG (for compatibility with older profiles).
    ensure_file(&dir.join("multi-action.svg"), MULTI_SVG);
    ensure_file(&dir.join("toggle-action.svg"), TOGGLE_SVG);
    ensure_png_from_svg(&dir.join("multi-action.png"), MULTI_SVG);
    ensure_png_from_svg(&dir.join("toggle-action.png"), TOGGLE_SVG);
}

/// Get whether or not the application is running inside the Flatpak sandbox.
pub fn is_flatpak() -> bool {
    var("FLATPAK_ID").is_ok()
        || var("container")
            .map(|x| x.to_lowercase().trim() == "flatpak")
            .unwrap_or(false)
}

/// Convert an icon specified in a plugin manifest to its full path.
pub fn convert_icon(path: String) -> String {
    // If the path already has a recognized image extension, use it as-is
    let lower = path.to_lowercase();
    if lower.ends_with(".png")
        || lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".gif")
        || lower.ends_with(".svg")
        || lower.ends_with(".bmp")
        || lower.ends_with(".webp")
    {
        return path;
    }

    // Otherwise, try to find a variant with an extension.
    // Prefer raster formats first so physical devices (which need pixels) can render icons reliably.
    // Many plugins ship both `.svg` and `.png` variants; UI/webviews can use SVG, but devices can't.
    if Path::new(&(path.clone() + "@2x.png")).exists() {
        path + "@2x.png"
    } else if Path::new(&(path.clone() + ".png")).exists() {
        path + ".png"
    } else if Path::new(&(path.clone() + ".jpg")).exists() {
        path + ".jpg"
    } else if Path::new(&(path.clone() + ".svg")).exists() {
        path + ".svg"
    } else {
        // Default to PNG (most common in Stream Deck plugins).
        path + ".png"
    }
}

#[derive(Clone, Copy, Serialize)]
pub struct FontSize(pub u16);
impl<'de> Deserialize<'de> for FontSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct MyVisitor;

        impl Visitor<'_> for MyVisitor {
            type Value = FontSize;

            fn expecting(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                fmt.write_str("integer or string")
            }

            fn visit_u64<E>(self, val: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(FontSize(val as u16))
            }

            fn visit_str<E>(self, val: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                match val.parse::<u64>() {
                    Ok(val) => self.visit_u64(val),
                    Err(_) => Err(E::custom("failed to parse integer")),
                }
            }
        }

        deserializer.deserialize_any(MyVisitor)
    }
}

/// A state of an action.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ActionState {
    #[serde(alias = "Image")]
    pub image: String,
    #[serde(alias = "Name")]
    pub name: String,
    #[serde(alias = "Title")]
    pub text: String,
    #[serde(alias = "ShowTitle")]
    pub show: bool,
    /// Whether the action's display name should be rendered as an additional label on the button.
    ///
    /// This is a RiverDeck extension (not part of the Stream Deck manifest schema).
    #[serde(default = "default_true")]
    pub show_action_name: bool,
    #[serde(alias = "TitleColor")]
    pub colour: String,
    #[serde(alias = "TitleAlignment")]
    pub alignment: String,
    /// Where the label text is placed on the button image.
    #[serde(default)]
    pub text_placement: TextPlacement,
    #[serde(alias = "FontFamily")]
    pub family: String,
    #[serde(alias = "FontStyle")]
    pub style: String,
    #[serde(alias = "FontSize")]
    pub size: FontSize,
    #[serde(alias = "FontUnderline")]
    pub underline: bool,
}

fn default_true() -> bool {
    true
}

impl Default for ActionState {
    fn default() -> Self {
        Self {
            image: "actionDefaultImage".to_owned(),
            name: String::new(),
            text: String::new(),
            show: true,
            show_action_name: true,
            colour: "#FFFFFF".to_owned(),
            alignment: "middle".to_owned(),
            text_placement: TextPlacement::Bottom,
            family: "Liberation Sans".to_owned(),
            style: "Regular".to_owned(),
            size: FontSize(16),
            underline: false,
        }
    }
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextPlacement {
    Top,
    #[default]
    Bottom,
    Left,
    Right,
}

#[serde_inline_default]
#[derive(Clone, Serialize, Deserialize)]
pub struct Category {
    pub icon: Option<String>,
    pub actions: Vec<Action>,
}

/// An action, deserialised from the plugin manifest.
#[serde_inline_default]
#[derive(Clone, Serialize, Deserialize)]
pub struct Action {
    #[serde(alias = "Name")]
    pub name: String,

    #[serde(alias = "UUID")]
    pub uuid: String,

    #[serde_inline_default(String::new())]
    pub plugin: String,

    #[serde_inline_default(String::new())]
    #[serde(alias = "Tooltip")]
    pub tooltip: String,

    #[serde_inline_default(String::new())]
    #[serde(alias = "Icon")]
    pub icon: String,

    #[serde_inline_default(false)]
    #[serde(alias = "DisableAutomaticStates")]
    pub disable_automatic_states: bool,

    #[serde_inline_default(true)]
    #[serde(alias = "VisibleInActionsList")]
    pub visible_in_action_list: bool,

    #[serde_inline_default(true)]
    #[serde(alias = "SupportedInMultiActions")]
    pub supported_in_multi_actions: bool,

    #[serde_inline_default(String::new())]
    #[serde(alias = "PropertyInspectorPath")]
    pub property_inspector: String,

    #[serde_inline_default(vec!["Keypad".to_owned()])]
    #[serde(alias = "Controllers")]
    pub controllers: Vec<String>,

    #[serde(alias = "States")]
    pub states: Vec<ActionState>,
}

/// Location metadata of a slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Context {
    pub device: String,
    pub profile: String,
    /// Stable page id within the selected profile (e.g. "1", "2", ...).
    pub page: String,
    pub controller: String,
    pub position: u8,
}

/// Information about the slot and index an instance is located in.
#[derive(
    Debug, Clone, PartialEq, Eq, serde_with::SerializeDisplay, serde_with::DeserializeFromStr,
)]
pub struct ActionContext {
    pub device: String,
    pub profile: String,
    /// Stable page id within the selected profile (e.g. "1", "2", ...).
    pub page: String,
    pub controller: String,
    pub position: u8,
    pub index: u16,
}

impl std::fmt::Display for ActionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}.{}.{}.{}.{}.{}",
            self.device, self.profile, self.page, self.controller, self.position, self.index
        )
    }
}

impl std::str::FromStr for ActionContext {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let segments: Vec<&str> = s.split('.').collect();
        // New format: device.profile.page.controller.position.index
        // Legacy format: device.profile.controller.position.index (page implied to be "1")
        if segments.len() < 5 {
            return Err(anyhow::anyhow!("not enough segments"));
        }
        let device = segments[0].to_owned();
        let profile = segments[1].to_owned();
        let (page, controller, position, index) = if segments.len() >= 6 {
            (
                segments[2].to_owned(),
                segments[3].to_owned(),
                u8::from_str(segments[4])?,
                u16::from_str(segments[5])?,
            )
        } else {
            (
                "1".to_owned(),
                segments[2].to_owned(),
                u8::from_str(segments[3])?,
                u16::from_str(segments[4])?,
            )
        };
        Ok(Self {
            device,
            profile,
            page,
            controller,
            position,
            index,
        })
    }
}

impl ActionContext {
    pub fn from_context(context: Context, index: u16) -> Self {
        Self {
            device: context.device,
            profile: context.profile,
            page: context.page,
            controller: context.controller,
            position: context.position,
            index,
        }
    }
}

impl From<ActionContext> for Context {
    fn from(value: ActionContext) -> Self {
        Self {
            device: value.device,
            profile: value.profile,
            page: value.page,
            controller: value.controller,
            position: value.position,
        }
    }
}

impl From<&ActionContext> for Context {
    fn from(value: &ActionContext) -> Self {
        Self::from(value.clone())
    }
}

/// An instance of an action.
#[derive(Clone, Serialize, Deserialize)]
pub struct ActionInstance {
    pub action: Action,
    pub context: ActionContext,
    pub states: Vec<ActionState>,
    pub current_state: u16,
    pub settings: serde_json::Value,
    pub children: Option<Vec<ActionInstance>>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Page {
    pub id: String,
    pub keys: Vec<Option<ActionInstance>>,
    pub sliders: Vec<Option<ActionInstance>>,
    #[serde(default)]
    pub encoder_screen_background: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub pages: Vec<Page>,
    #[serde(default = "default_selected_page")]
    pub selected_page: String,
}

fn default_selected_page() -> String {
    "1".to_owned()
}

/// A map of category names to a list of actions in that category.
pub static CATEGORIES: Lazy<RwLock<HashMap<String, Category>>> = Lazy::new(|| {
    let mut hashmap = HashMap::new();
    hashmap.insert(
        PRODUCT_NAME.to_owned(),
        Category {
            icon: None,
            actions: vec![
                serde_json::from_value(serde_json::json!(
                    {
                        "name": "Multi Action",
                        "icon": "riverdeck/multi-action.png",
                        "plugin": "riverdeck",
                        "uuid": "riverdeck.multiaction",
                        "tooltip": "Execute multiple actions",
                        "controllers": [ "Keypad" ],
                        "states": [ { "image": "riverdeck/multi-action.png" } ],
                        "supported_in_multi_actions": false
                    }
                ))
                .unwrap(),
                serde_json::from_value(serde_json::json!(
                    {
                        "name": "Toggle Action",
                        "icon": "riverdeck/toggle-action.png",
                        "plugin": "riverdeck",
                        "uuid": "riverdeck.toggleaction",
                        "tooltip": "Cycle through multiple actions",
                        "controllers": [ "Keypad" ],
                        "states": [ { "image": "riverdeck/toggle-action.png" } ],
                        "supported_in_multi_actions": false
                    }
                ))
                .unwrap(),
            ],
        },
    );
    RwLock::new(hashmap)
});
