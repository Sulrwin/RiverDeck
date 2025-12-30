//! Duplicates of many structs to facilitate saving profiles to disk in a format that can be transferred between devices or systems.

use crate::shared::{Action, ActionContext, ActionInstance, ActionState, Page, Profile};

use std::{
    fs,
    path::{Path, PathBuf},
};

use path_slash::{PathBufExt, PathExt};
use serde::{Deserialize, Serialize};

#[derive(serde_with::SerializeDisplay, serde_with::DeserializeFromStr)]
pub struct DiskActionContext {
    pub controller: String,
    pub position: u8,
    pub index: u16,
}

impl std::fmt::Display for DiskActionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.controller, self.position, self.index)
    }
}

impl std::str::FromStr for DiskActionContext {
    type Err = std::num::ParseIntError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let segments: Vec<&str> = s.split('.').collect();
        let mut offset: usize = 0;
        if segments.len() == 5 {
            offset = 2;
        }
        let controller = segments[offset].to_owned();
        let position = u8::from_str(segments[1 + offset])?;
        let index = u16::from_str(segments[2 + offset])?;
        Ok(Self {
            controller,
            position,
            index,
        })
    }
}

impl From<ActionContext> for DiskActionContext {
    fn from(value: ActionContext) -> Self {
        Self {
            controller: value.controller,
            position: value.position,
            index: value.index,
        }
    }
}

impl DiskActionContext {
    fn into_action_context(self, device: String, profile: String, page: String) -> ActionContext {
        ActionContext {
            device,
            profile,
            page,
            controller: self.controller,
            position: self.position,
            index: self.index,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct DiskActionInstance {
    pub action: Action,
    pub context: DiskActionContext,
    pub states: Vec<ActionState>,
    pub current_state: u16,
    pub settings: serde_json::Value,
    pub children: Option<Vec<DiskActionInstance>>,
}

impl From<ActionInstance> for DiskActionInstance {
    fn from(mut value: ActionInstance) -> Self {
        let disk_context: DiskActionContext = value.context.clone().into();
        let config_dir = crate::shared::config_dir();
        let image_dir = config_dir
            .join("images")
            .join(&value.context.device)
            .join(&value.context.profile)
            .join(&value.context.page)
            .join(disk_context.to_string());

        let normalise_path = |value: &str| -> String {
            let path = Path::new(value);
            if path.starts_with(&image_dir) {
                path.strip_prefix(&image_dir)
                    .unwrap()
                    .to_slash_lossy()
                    .into_owned()
            } else if path.starts_with(&config_dir) {
                path.strip_prefix(&config_dir)
                    .unwrap()
                    .to_slash_lossy()
                    .into_owned()
            } else {
                path.to_slash_lossy().into_owned()
            }
        };

        for (index, state) in value.states.iter_mut().enumerate() {
            if state.image.trim() == "data:" {
                state.image = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIW2NgYGD4DwABBAEAwS2OUAAAAABJRU5ErkJggg==".to_owned();
            }

            if state.image.starts_with("data:") {
                let mut extension = state
                    .image
                    .split_once('/')
                    .unwrap()
                    .1
                    .split_once(',')
                    .unwrap()
                    .0;
                if extension.contains(';') {
                    extension = extension.split_once(';').unwrap().0;
                }
                if extension.contains('+') {
                    extension = extension.split_once('+').unwrap().0;
                }

                let data = if state.image.contains(";base64,") {
                    use base64::Engine;
                    let Ok(data) = base64::engine::general_purpose::STANDARD
                        .decode(state.image.split_once(";base64,").unwrap().1)
                    else {
                        continue;
                    };
                    data
                } else {
                    state.image.split_once(',').unwrap().1.as_bytes().to_vec()
                };

                let filename = format!("{}.{}", index, extension);
                if fs::create_dir_all(&image_dir).is_err()
                    || fs::write(image_dir.join(&filename), data).is_err()
                {
                    continue;
                };
                state.image = filename;
            }

            state.image = normalise_path(&state.image);
        }
        for state in value.action.states.iter_mut() {
            state.image = normalise_path(&state.image);
        }
        value.action.icon = normalise_path(&value.action.icon);
        value.action.property_inspector = normalise_path(&value.action.property_inspector);

        Self {
            context: disk_context,
            action: value.action,
            states: value.states,
            current_state: value.current_state,
            settings: value.settings,
            children: value
                .children
                .map(|c| c.into_iter().map(|v| v.into()).collect()),
        }
    }
}

impl DiskActionInstance {
    fn into_action_instance(self, path: &Path, page: &str) -> ActionInstance {
        let config_dir = crate::shared::config_dir();
        let mut iter = path.strip_prefix(&config_dir).unwrap().iter();
        let device = iter.nth(1).unwrap().to_string_lossy().into_owned();
        let mut profile = iter
            .map(|x| x.to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        profile = profile[..profile.len() - 5].to_owned();

        let reconstruct_path = |value: &str| -> String {
            if !(value.is_empty()
                || value.starts_with("data:")
                || value.starts_with("riverdeck/")
                || value.starts_with("opendeck/"))
            {
                config_dir
                    .join(PathBuf::from_slash(value))
                    .to_string_lossy()
                    .into_owned()
            } else {
                value.to_owned()
            }
        };

        let mut states = self.states.clone();
        for state in states.iter_mut() {
            crate::shared::normalize_starterpack_paths(&mut state.image);
            if let Some(true) = state.image.chars().next().map(|v| v.is_numeric()) {
                // New location: images/<device>/<profile>/<page>/<context>/<file>
                // Legacy location: images/<device>/<profile>/<context>/<file>
                let new_path = config_dir
                    .join("images")
                    .join(&device)
                    .join(&profile)
                    .join(page)
                    .join(self.context.to_string())
                    .join(&state.image);
                let legacy_path = config_dir
                    .join("images")
                    .join(&device)
                    .join(&profile)
                    .join(self.context.to_string())
                    .join(&state.image);
                let chosen = if new_path.is_file() {
                    new_path
                } else {
                    legacy_path
                };
                state.image = chosen.to_string_lossy().into_owned();
            } else {
                state.image = reconstruct_path(&state.image);
            }
        }
        let mut action = self.action.clone();
        for state in action.states.iter_mut() {
            crate::shared::normalize_starterpack_paths(&mut state.image);
            state.image = reconstruct_path(&state.image);
        }
        crate::shared::normalize_starterpack_paths(&mut action.icon);
        action.icon = reconstruct_path(&action.icon);
        crate::shared::normalize_starterpack_paths(&mut action.property_inspector);
        action.property_inspector = reconstruct_path(&action.property_inspector);

        // Normalize legacy OpenDeck built-in actions to RiverDeck equivalents.
        crate::shared::normalize_builtin_action(&mut action.plugin, &mut action.uuid);
        crate::shared::normalize_starterpack_action(&mut action.plugin, &mut action.uuid);

        ActionInstance {
            context: self
                .context
                .into_action_context(device, profile, page.to_owned()),
            action,
            states,
            current_state: self.current_state,
            settings: self.settings,
            children: self.children.map(|c| {
                c.into_iter()
                    .map(|v| v.into_action_instance(path, page))
                    .collect()
            }),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct DiskPage {
    pub id: String,
    pub keys: Vec<Option<DiskActionInstance>>,
    pub sliders: Vec<Option<DiskActionInstance>>,
    #[serde(default)]
    pub encoder_screen_background: Option<String>,
    #[serde(default)]
    pub encoder_screen_crop: Option<crate::shared::EncoderScreenCrop>,
}

impl From<&Page> for DiskPage {
    fn from(value: &Page) -> Self {
        let config_dir = crate::shared::config_dir();
        let normalise_path = |value: &str| -> String {
            let path = Path::new(value);
            if path.starts_with(&config_dir) {
                path.strip_prefix(&config_dir)
                    .unwrap()
                    .to_slash_lossy()
                    .into_owned()
            } else {
                path.to_slash_lossy().into_owned()
            }
        };

        Self {
            id: value.id.clone(),
            keys: value
                .keys
                .clone()
                .into_iter()
                .map(|x| x.map(|v| v.into()))
                .collect(),
            sliders: value
                .sliders
                .clone()
                .into_iter()
                .map(|x| x.map(|v| v.into()))
                .collect(),
            encoder_screen_background: value
                .encoder_screen_background
                .as_deref()
                .map(normalise_path),
            encoder_screen_crop: value.encoder_screen_crop,
        }
    }
}

impl DiskPage {
    fn into_page(self, path: &Path) -> Page {
        let config_dir = crate::shared::config_dir();
        let reconstruct_path = |value: &str| -> String {
            if !(value.is_empty()
                || value.starts_with("data:")
                || value.starts_with("riverdeck/")
                || value.starts_with("opendeck/"))
            {
                config_dir
                    .join(PathBuf::from_slash(value))
                    .to_string_lossy()
                    .into_owned()
            } else {
                value.to_owned()
            }
        };

        let page_id = self.id.clone();
        Page {
            id: self.id,
            keys: self
                .keys
                .into_iter()
                .map(|x| x.map(|v| v.into_action_instance(path, &page_id)))
                .collect(),
            sliders: self
                .sliders
                .into_iter()
                .map(|x| x.map(|v| v.into_action_instance(path, &page_id)))
                .collect(),
            encoder_screen_background: self
                .encoder_screen_background
                .as_deref()
                .map(reconstruct_path),
            encoder_screen_crop: self.encoder_screen_crop,
        }
    }
}

impl super::FromAndIntoDiskValue for Profile {
    fn into_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        let disk: DiskProfileV2 = self.into();
        serde_json::to_value(disk)
    }
    fn from_value(value: serde_json::Value, path: &Path) -> Result<Profile, serde_json::Error> {
        // Prefer the new format, but accept the legacy single-page format for backwards compatibility.
        if let Ok(disk) = serde_json::from_value::<DiskProfileV2>(value.clone()) {
            Ok(disk.into_profile(path))
        } else {
            let legacy: DiskProfileLegacy = serde_json::from_value(value)?;
            Ok(legacy.into_profile(path))
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct DiskProfileV2 {
    #[serde(default)]
    pub selected_page: Option<String>,
    pub pages: Vec<DiskPage>,
}

impl From<&Profile> for DiskProfileV2 {
    fn from(value: &Profile) -> Self {
        Self {
            selected_page: Some(value.selected_page.clone()),
            pages: value.pages.iter().map(|p| p.into()).collect(),
        }
    }
}

impl DiskProfileV2 {
    fn into_profile(self, path: &Path) -> Profile {
        let config_dir = crate::shared::config_dir();
        let mut iter = path.strip_prefix(&config_dir).unwrap().iter();
        let _ = iter.nth(1);
        let mut id = iter
            .map(|x| x.to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        id = id[..id.len() - 5].to_owned();

        let mut pages: Vec<Page> = self.pages.into_iter().map(|p| p.into_page(path)).collect();
        if pages.is_empty() {
            pages.push(Page {
                id: "1".to_owned(),
                keys: vec![],
                sliders: vec![],
                encoder_screen_background: None,
                encoder_screen_crop: None,
            });
        }
        let selected_page = self.selected_page.unwrap_or_else(|| "1".to_owned());

        Profile {
            id,
            pages,
            selected_page,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct DiskProfileLegacy {
    pub keys: Vec<Option<DiskActionInstance>>,
    pub sliders: Vec<Option<DiskActionInstance>>,
    #[serde(default)]
    pub encoder_screen_background: Option<String>,
}

impl DiskProfileLegacy {
    fn into_profile(self, path: &Path) -> Profile {
        let config_dir = crate::shared::config_dir();
        let mut iter = path.strip_prefix(&config_dir).unwrap().iter();
        let _ = iter.nth(1);
        let mut id = iter
            .map(|x| x.to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        id = id[..id.len() - 5].to_owned();

        let page_id = "1".to_owned();
        Profile {
            id,
            selected_page: page_id.clone(),
            pages: vec![Page {
                id: page_id.clone(),
                keys: self
                    .keys
                    .into_iter()
                    .map(|x| x.map(|v| v.into_action_instance(path, &page_id)))
                    .collect(),
                sliders: self
                    .sliders
                    .into_iter()
                    .map(|x| x.map(|v| v.into_action_instance(path, &page_id)))
                    .collect(),
                encoder_screen_background: self.encoder_screen_background,
                encoder_screen_crop: None,
            }],
        }
    }
}
