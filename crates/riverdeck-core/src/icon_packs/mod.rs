use std::path::{Path, PathBuf};

use anyhow::anyhow;

pub mod index;
pub mod manifest;

#[derive(Debug, Clone)]
pub struct IconPackInfo {
    pub id: String,
    pub name: String,
    pub author: String,
    pub version: String,
    /// Root folder containing `manifest.json` and `icons.json`.
    pub root: PathBuf,
    /// True if this pack comes from `resource_dir` (bundled).
    pub builtin: bool,
}

#[derive(Debug, Clone)]
pub struct IconInfo {
    pub pack_id: String,
    pub path: String,
    pub name: String,
    pub tags: Vec<String>,
    /// Absolute filesystem path when applicable.
    pub resolved_path: PathBuf,
}

pub fn config_icon_packs_dir() -> PathBuf {
    crate::shared::config_dir().join("icon_packs")
}

fn resource_icon_packs_dirs() -> Vec<PathBuf> {
    let Some(res) = crate::shared::resource_dir() else {
        return vec![];
    };
    // Prefer a dedicated `icon_packs/` root, but also support the plan's `packaging/icon_packs/`
    // location in dev trees.
    vec![
        res.join("icon_packs"),
        res.join("packaging").join("icon_packs"),
    ]
}

pub fn read_manifest(pack_root: &Path) -> Result<manifest::IconPackManifest, anyhow::Error> {
    let bytes = std::fs::read(pack_root.join("manifest.json"))?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn read_index(pack_root: &Path) -> Result<index::IconPackIndex, anyhow::Error> {
    let bytes = std::fs::read(pack_root.join("icons.json"))?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn normalize_pack_dir_name(name: &str) -> bool {
    name.to_lowercase().ends_with(".sdiconpack")
}

fn list_from_dir(base: &Path, builtin: bool) -> Vec<IconPackInfo> {
    let mut out = vec![];
    let Ok(entries) = std::fs::read_dir(base) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() || !meta.is_dir() {
            continue;
        }
        let Some(id) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !normalize_pack_dir_name(id) {
            continue;
        }

        let Ok(m) = read_manifest(&path) else {
            continue;
        };
        out.push(IconPackInfo {
            id: id.to_owned(),
            name: if m.name.trim().is_empty() {
                id.to_owned()
            } else {
                m.name
            },
            author: m.author,
            version: m.version,
            root: path,
            builtin,
        });
    }
    out
}

pub fn list_icon_packs() -> Vec<IconPackInfo> {
    let mut packs = vec![];
    packs.extend(list_from_dir(&config_icon_packs_dir(), false));
    for d in resource_icon_packs_dirs() {
        packs.extend(list_from_dir(&d, true));
    }
    // Stable ordering for UI.
    packs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    packs
}

fn resolve_icon_path(pack_root: &Path, entry_path: &str) -> Option<PathBuf> {
    let p = entry_path.trim();
    if p.is_empty() {
        return None;
    }
    // RiverDeck "virtual" icon pack entries can reference already-known built-in paths.
    if p.starts_with("riverdeck/") || p.starts_with("opendeck/") {
        let cfg = crate::shared::config_dir().join(p);
        if cfg.is_file() {
            return Some(cfg);
        }
        if let Some(res) = crate::shared::resource_dir() {
            let rp = res.join(p);
            if rp.is_file() {
                return Some(rp);
            }
        }
        return None;
    }

    let joined = pack_root.join(p);
    // Avoid `..` escapes.
    let Ok(root_canon) = std::fs::canonicalize(pack_root) else {
        return None;
    };
    let Ok(joined_canon) = std::fs::canonicalize(&joined) else {
        return None;
    };
    if !joined_canon.starts_with(root_canon) {
        return None;
    }
    if joined_canon.is_file() {
        Some(joined_canon)
    } else {
        None
    }
}

pub fn list_icons_for_pack(pack: &IconPackInfo) -> Result<Vec<IconInfo>, anyhow::Error> {
    let idx = read_index(&pack.root)?;
    let mut out = Vec::with_capacity(idx.icons.len());
    for icon in idx.icons {
        let Some(resolved) = resolve_icon_path(&pack.root, &icon.path) else {
            continue;
        };
        out.push(IconInfo {
            pack_id: pack.id.clone(),
            path: icon.path,
            name: icon.name,
            tags: icon.tags,
            resolved_path: resolved,
        });
    }
    Ok(out)
}

pub fn get_pack_by_id(pack_id: &str) -> Result<IconPackInfo, anyhow::Error> {
    list_icon_packs()
        .into_iter()
        .find(|p| p.id == pack_id)
        .ok_or_else(|| anyhow!("icon pack not found"))
}

pub fn list_icons(pack_id: &str) -> Result<Vec<IconInfo>, anyhow::Error> {
    let pack = get_pack_by_id(pack_id)?;
    list_icons_for_pack(&pack)
}

pub fn validate_pack_id(id: &str) -> Result<(), anyhow::Error> {
    if id.trim().is_empty() {
        return Err(anyhow!("icon pack id is empty"));
    }
    if !normalize_pack_dir_name(id) {
        return Err(anyhow!("invalid icon pack id (must end with .sdIconPack)"));
    }
    Ok(())
}
