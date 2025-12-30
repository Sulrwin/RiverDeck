use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::plugins::manifest;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginSource {
    Workspace,
    Config,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginSupport {
    Supported,
    Unsupported(String),
}

#[derive(Debug, Clone)]
pub struct LocalPluginEntry {
    pub id: String, // folder name, e.g. "io.github.sulrwin.riverdeck.starterpack.sdPlugin"
    pub name: String,
    pub version: Option<String>,
    pub icon: Option<String>,
    pub source: PluginSource,
    pub path: PathBuf,
    pub installed: bool,
    pub enabled: bool,
    pub support: PluginSupport,
}

fn workspace_plugins_dir() -> Option<PathBuf> {
    // Local-only marketplace default: look for `./plugins` when run from a repo checkout.
    // This keeps production behavior unchanged (most users won't have a `plugins/` CWD).
    let cwd = std::env::current_dir().ok()?;
    let p = cwd.join("plugins");
    if p.is_dir() { Some(p) } else { None }
}

fn config_plugins_dir() -> PathBuf {
    crate::shared::config_dir().join("plugins")
}

fn list_plugin_dirs(root: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    let Ok(entries) = fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Ok(meta) = fs::metadata(&path)
            && meta.is_dir()
        {
            out.push(path);
        }
    }
    out
}

fn read_manifest_for_marketplace(
    plugin_dir: &Path,
    source: PluginSource,
) -> Result<(manifest::PluginManifest, PathBuf), anyhow::Error> {
    // For installed/config plugins, we require the runtime layout: `<plugin>/manifest.json`.
    // For workspace plugins (repo checkout), we support the source layout: `<plugin>/assets/manifest.json`.
    match source {
        PluginSource::Config => Ok((
            manifest::read_manifest(plugin_dir)?,
            plugin_dir.to_path_buf(),
        )),
        PluginSource::Workspace => {
            let root_manifest = plugin_dir.join("manifest.json");
            if root_manifest.is_file() {
                return Ok((
                    manifest::read_manifest(plugin_dir)?,
                    plugin_dir.to_path_buf(),
                ));
            }

            let assets_manifest = plugin_dir.join("assets").join("manifest.json");
            if assets_manifest.is_file() {
                let raw = fs::read(&assets_manifest)?;
                let m: manifest::PluginManifest = serde_json::from_slice(&raw)
                    .map_err(|e| anyhow::anyhow!("failed to parse assets/manifest.json: {e}"))?;
                // Resolve icon/PI paths relative to `<plugin>/assets`.
                return Ok((m, plugin_dir.join("assets")));
            }

            Err(anyhow::anyhow!(
                "missing manifest.json (expected manifest.json or assets/manifest.json)"
            ))
        }
    }
}

pub fn list_local_plugins() -> Vec<LocalPluginEntry> {
    let disabled = crate::store::get_settings()
        .ok()
        .map(|s| s.value.disabled_plugins)
        .unwrap_or_default();

    let config_dir = config_plugins_dir();
    let config_plugins = list_plugin_dirs(&config_dir);
    let workspace_plugins = workspace_plugins_dir()
        .as_deref()
        .map(list_plugin_dirs)
        .unwrap_or_default();

    // Merge by plugin folder name (ID). Prefer Config as "installed".
    let mut by_id: BTreeMap<String, LocalPluginEntry> = BTreeMap::new();

    for path in workspace_plugins {
        let Some(id) = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_owned())
        else {
            continue;
        };

        let (name, version, icon, support) =
            match read_manifest_for_marketplace(&path, PluginSource::Workspace) {
                Ok((m, base)) => {
                    let support = match manifest::validate_riverdeck_native(&id, &m) {
                        Ok(()) => PluginSupport::Supported,
                        Err(r) => PluginSupport::Unsupported(format!("{r:?}")),
                    };
                    let icon = Some(crate::shared::convert_icon(
                        base.join(&m.icon).to_string_lossy().to_string(),
                    ));
                    (m.name, Some(m.version), icon, support)
                }
                Err(e) => (
                    id.clone(),
                    None,
                    None,
                    PluginSupport::Unsupported(e.to_string()),
                ),
            };

        by_id.insert(
            id.clone(),
            LocalPluginEntry {
                enabled: !disabled.contains(&id),
                id,
                name,
                version,
                icon,
                source: PluginSource::Workspace,
                path,
                installed: false,
                support,
            },
        );
    }

    for path in config_plugins {
        let Some(id) = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_owned())
        else {
            continue;
        };

        let (name, version, icon, support) =
            match read_manifest_for_marketplace(&path, PluginSource::Config) {
                Ok((m, base)) => {
                    let support = match manifest::validate_riverdeck_native(&id, &m) {
                        Ok(()) => PluginSupport::Supported,
                        Err(r) => PluginSupport::Unsupported(format!("{r:?}")),
                    };
                    let icon = Some(crate::shared::convert_icon(
                        base.join(&m.icon).to_string_lossy().to_string(),
                    ));
                    (m.name, Some(m.version), icon, support)
                }
                Err(e) => (
                    id.clone(),
                    None,
                    None,
                    PluginSupport::Unsupported(e.to_string()),
                ),
            };

        by_id.insert(
            id.clone(),
            LocalPluginEntry {
                id: id.clone(),
                name,
                version,
                icon,
                source: PluginSource::Config,
                path,
                installed: true,
                enabled: !disabled.contains(&id),
                support,
            },
        );
    }

    by_id.into_values().collect()
}

pub fn set_plugin_enabled(plugin_id: &str, enabled: bool) -> anyhow::Result<()> {
    let mut store = crate::store::get_settings()?;
    let list = &mut store.value.disabled_plugins;
    if enabled {
        list.retain(|p| p != plugin_id);
    } else if !list.contains(&plugin_id.to_owned()) {
        list.push(plugin_id.to_owned());
    }
    store.save()?;
    Ok(())
}

pub fn install_from_workspace(plugin_id: &str) -> anyhow::Result<()> {
    let Some(root) = workspace_plugins_dir() else {
        anyhow::bail!("workspace plugins directory not found");
    };
    let src = root.join(plugin_id);
    if !src.is_dir() {
        anyhow::bail!("workspace plugin not found: {}", src.display());
    }

    let dst_root = config_plugins_dir();
    fs::create_dir_all(&dst_root)?;
    let dst = dst_root.join(plugin_id);

    if dst.exists() {
        // Best-effort overwrite: move existing to .old.
        let old = dst.with_extension("old");
        let _ = fs::remove_dir_all(&old);
        fs::rename(&dst, &old)?;
    }

    crate::shared::copy_dir(&src, &dst)
        .map_err(|_| anyhow::anyhow!("failed to copy plugin directory"))?;

    // If the workspace plugin uses the source layout (`assets/*`), flatten it into the runtime layout.
    // Runtime expects:
    //   <plugin>/manifest.json
    //   <plugin>/icons/*
    //   <plugin>/propertyInspector/*
    let dst_manifest = dst.join("manifest.json");
    let dst_assets = dst.join("assets");
    let dst_assets_manifest = dst_assets.join("manifest.json");
    if !dst_manifest.exists() && dst_assets_manifest.is_file() {
        fs::copy(&dst_assets_manifest, &dst_manifest)?;

        let assets_icons = dst_assets.join("icons");
        let root_icons = dst.join("icons");
        if assets_icons.is_dir() && !root_icons.exists() {
            let _ = crate::shared::copy_dir(&assets_icons, &root_icons);
        }

        let assets_pi = dst_assets.join("propertyInspector");
        let root_pi = dst.join("propertyInspector");
        if assets_pi.is_dir() && !root_pi.exists() {
            let _ = crate::shared::copy_dir(&assets_pi, &root_pi);
        }
    }

    // Best-effort: if the plugin is a Rust workspace member, try to copy its built binary
    // into `CodePathLin` so it can be launched immediately after install.
    if let Ok(m) = manifest::read_manifest(&dst)
        && let Some(code_path) = m.code_path_linux.as_deref()
        && code_path.starts_with("linux/")
    {
        let rel = Path::new(code_path);
        let bin_name = rel.file_name().and_then(|s| s.to_str()).unwrap_or_default();
        if !bin_name.is_empty() {
            let workspace_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let candidates = [
                workspace_root.join("target").join("debug").join(bin_name),
                workspace_root.join("target").join("release").join(bin_name),
            ];
            for c in candidates {
                if c.is_file() {
                    let out_path = dst.join(rel);
                    if let Some(parent) = out_path.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    let _ = fs::copy(&c, &out_path);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = fs::set_permissions(&out_path, fs::Permissions::from_mode(0o755));
                    }
                    break;
                }
            }
        }
    }
    Ok(())
}
