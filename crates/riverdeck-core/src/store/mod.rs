pub mod profiles;
mod simplified_profile;

use crate::shared::is_flatpak;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

pub(crate) fn pretty_json_enabled() -> bool {
    // Debug/dev convenience: opt-in pretty JSON on disk.
    // Any truthy value enables it: "1", "true", "yes", "on".
    let v = std::env::var("RIVERDECK_PRETTY_JSON").unwrap_or_default();
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

pub(crate) fn json_value_to_bytes(value: &serde_json::Value) -> Result<Vec<u8>, serde_json::Error> {
    if pretty_json_enabled() {
        serde_json::to_vec_pretty(value)
    } else {
        serde_json::to_vec(value)
    }
}

pub(crate) fn write_atomic_bytes(path: &Path, contents: &[u8]) -> Result<(), anyhow::Error> {
    fs::create_dir_all(path.parent().unwrap())?;

    let temp_path = path.with_extension("json.temp");
    let backup_path = path.with_extension("json.bak");

    if let Ok(meta) = fs::symlink_metadata(&temp_path)
        && meta.file_type().is_symlink()
    {
        return Err(anyhow::anyhow!("refusing to write to symlinked temp file"));
    }
    if let Ok(meta) = fs::symlink_metadata(&backup_path)
        && meta.file_type().is_symlink()
    {
        return Err(anyhow::anyhow!(
            "refusing to write to symlinked backup file"
        ));
    }
    if let Ok(meta) = fs::symlink_metadata(path)
        && meta.file_type().is_symlink()
    {
        return Err(anyhow::anyhow!(
            "refusing to overwrite symlinked store file"
        ));
    }

    // Write to temporary file
    let mut temp_file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temp_path)?;
    FileExt::lock_exclusive(&temp_file)?;
    temp_file.write_all(contents)?;
    temp_file.sync_all()?;
    FileExt::unlock(&temp_file)?;
    drop(temp_file);

    // If main file exists, back it up
    if path.exists() {
        fs::rename(path, &backup_path)?;
    }

    // Rename temp file to main file
    fs::rename(&temp_path, path)?;

    // Remove backup file if everything succeeded
    if backup_path.exists() {
        let _ = fs::remove_file(&backup_path);
    }

    Ok(())
}

pub trait FromAndIntoDiskValue
where
    Self: Sized,
{
    #[allow(clippy::wrong_self_convention)]
    fn into_value(&self) -> Result<serde_json::Value, serde_json::Error>;
    fn from_value(_: serde_json::Value, _: &Path) -> Result<Self, serde_json::Error>;
}

pub trait NotProfile {}

impl<T> FromAndIntoDiskValue for T
where
    T: Serialize + for<'a> Deserialize<'a> + NotProfile,
{
    fn into_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(self)
    }
    fn from_value(value: serde_json::Value, _: &Path) -> Result<T, serde_json::Error> {
        serde_json::from_value(value)
    }
}

/// Allows for easy persistence of values using JSON files
pub struct Store<T>
where
    T: FromAndIntoDiskValue,
{
    pub value: T,
    path: PathBuf,
}

impl<T> Store<T>
where
    T: FromAndIntoDiskValue,
{
    /// Validate that a file contains valid data for type T
    fn validate_file_contents(path: &Path) -> Result<T, anyhow::Error> {
        if let Ok(meta) = fs::symlink_metadata(path)
            && meta.file_type().is_symlink()
        {
            return Err(anyhow::anyhow!("refusing to read symlinked store file"));
        }
        let file_contents = fs::read(path)?;
        let value: T = T::from_value(serde_json::from_slice(&file_contents)?, path)?;
        Ok(value)
    }

    /// Create a new Store given an ID and storage directory
    pub fn new(id: &str, config_dir: &Path, default: T) -> Result<Self, anyhow::Error> {
        let path = config_dir.join(format!("{}.json", id));
        let temp_path = path.with_extension("json.temp");
        let backup_path = path.with_extension("json.bak");

        if let Ok(value) = Self::validate_file_contents(&path) {
            let _ = fs::remove_file(&temp_path);
            let _ = fs::remove_file(&backup_path);
            Ok(Self { path, value })
        } else if let Ok(value) = Self::validate_file_contents(&temp_path) {
            fs::rename(&temp_path, &path)?;
            Ok(Self { path, value })
        } else if let Ok(value) = Self::validate_file_contents(&backup_path) {
            fs::rename(&backup_path, &path)?;
            Ok(Self { path, value })
        } else {
            Ok(Self {
                path,
                value: default,
            })
        }
    }

    /// Save the relevant Store as a file
    pub fn save(&self) -> Result<(), anyhow::Error> {
        let value = T::into_value(&self.value)?;
        let bytes = json_value_to_bytes(&value)?;
        write_atomic_bytes(&self.path, &bytes)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(default, from = "SettingsSerde")]
pub struct Settings {
    pub version: String,
    pub language: String,
    pub brightness: u8,
    /// UI theme identifier (host UI only). Uses stable string IDs so new themes can be added later
    /// without breaking old configs.
    pub theme: ThemeId,
    /// Legacy: used by older configs. Migrated into `theme` on load and not written back out.
    #[serde(skip_serializing)]
    pub darktheme: bool,
    pub background: bool,
    pub autolaunch: bool,
    /// Placeholder toggle for future screensaver integration.
    pub screensaver: bool,
    pub updatecheck: bool,
    pub statistics: bool,
    pub separatewine: bool,
    pub developer: bool,
    pub disableelgato: bool,
    /// Plugin IDs (folder names) that should not be launched at startup.
    pub disabled_plugins: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: "0.0.0".to_owned(),
            language: "en".to_owned(),
            brightness: 50,
            theme: ThemeId::river_dark(),
            darktheme: true,
            background: !is_flatpak(),
            autolaunch: false,
            screensaver: false,
            updatecheck: option_env!("RIVERDECK_DISABLE_UPDATE_CHECK").is_none() && !is_flatpak(),
            // RiverDeck does not enable telemetry by default.
            statistics: false,
            separatewine: false,
            developer: false,
            disableelgato: false,
            disabled_plugins: vec![],
        }
    }
}

impl NotProfile for Settings {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ThemeId(pub String);

impl ThemeId {
    pub const RIVER_DARK_ID: &'static str = "river-dark";
    pub const RIVER_LIGHT_ID: &'static str = "river-light";

    pub fn river_dark() -> Self {
        Self(Self::RIVER_DARK_ID.to_owned())
    }

    pub fn river_light() -> Self {
        Self(Self::RIVER_LIGHT_ID.to_owned())
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SettingsSerde {
    version: String,
    language: String,
    brightness: u8,
    theme: Option<ThemeId>,
    darktheme: Option<bool>,
    background: bool,
    autolaunch: bool,
    screensaver: bool,
    updatecheck: bool,
    statistics: bool,
    separatewine: bool,
    developer: bool,
    disableelgato: bool,
    disabled_plugins: Vec<String>,
}

impl From<SettingsSerde> for Settings {
    fn from(s: SettingsSerde) -> Self {
        let theme = s.theme.unwrap_or_else(|| {
            if s.darktheme == Some(false) {
                ThemeId::river_light()
            } else {
                ThemeId::river_dark()
            }
        });
        let darktheme = theme.0 == ThemeId::RIVER_DARK_ID;

        Self {
            version: s.version,
            language: s.language,
            brightness: s.brightness,
            theme,
            darktheme,
            background: s.background,
            autolaunch: s.autolaunch,
            screensaver: s.screensaver,
            updatecheck: s.updatecheck,
            statistics: s.statistics,
            separatewine: s.separatewine,
            developer: s.developer,
            disableelgato: s.disableelgato,
            disabled_plugins: s.disabled_plugins,
        }
    }
}

pub fn get_settings() -> Result<Store<Settings>, anyhow::Error> {
    Store::new(
        "settings",
        &crate::shared::config_dir(),
        Settings::default(),
    )
}
