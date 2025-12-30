use crate::shared::config_dir;

#[derive(Debug, Clone, serde::Serialize)]
pub struct IconPackInfo {
    pub id: String,
    pub name: String,
    pub author: String,
    pub version: String,
    pub builtin: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct IconInfo {
    pub pack_id: String,
    pub name: String,
    pub tags: Vec<String>,
    /// Absolute file path for the icon (RiverDeck will copy this to the per-button images dir).
    pub path: String,
}

pub fn list_icon_packs() -> Vec<IconPackInfo> {
    crate::icon_packs::list_icon_packs()
        .into_iter()
        .map(|p| IconPackInfo {
            id: p.id,
            name: p.name,
            author: p.author,
            version: p.version,
            builtin: p.builtin,
        })
        .collect()
}

pub fn list_icons(pack_id: &str, query: Option<&str>) -> Result<Vec<IconInfo>, anyhow::Error> {
    let mut icons = crate::icon_packs::list_icons(pack_id)?;
    let q = query.unwrap_or("").trim().to_lowercase();
    if !q.is_empty() {
        icons.retain(|i| {
            i.name.to_lowercase().contains(&q)
                || i.tags.iter().any(|t| t.to_lowercase().contains(&q))
        });
    }
    Ok(icons
        .into_iter()
        .map(|i| IconInfo {
            pack_id: i.pack_id,
            name: i.name,
            tags: i.tags,
            path: i.resolved_path.to_string_lossy().into_owned(),
        })
        .collect())
}

pub async fn install_icon_pack(
    url: String,
    fallback_id: Option<String>,
) -> Result<String, anyhow::Error> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(anyhow::anyhow!("unsupported url scheme"));
    }

    let resp = reqwest::get(url).await?;
    use std::ops::Deref;
    let bytes = resp.bytes().await?.deref().to_owned();

    let id =
        match crate::zip_extract::dir_name_with_suffix(std::io::Cursor::new(&bytes), ".sdiconpack")
        {
            Ok(id) => id,
            Err(err) => match fallback_id {
                Some(id) => {
                    if id.to_lowercase().ends_with(".sdiconpack") {
                        id
                    } else {
                        format!("{id}.sdIconPack")
                    }
                }
                None => return Err(err.into()),
            },
        };

    crate::icon_packs::validate_pack_id(&id)?;

    let result: Result<(), anyhow::Error> = async {
        let config_dir = config_dir();
        let packs_dir = config_dir.join("icon_packs");
        let actual = packs_dir.join(&id);

        // Extract into isolated temp dir, then move into place.
        let temp_root = config_dir.join("temp");
        tokio::fs::create_dir_all(&temp_root).await?;
        let extract_root = temp_root.join(format!(
            "extract_iconpack_{}_{}",
            id.replace('/', "_"),
            std::process::id()
        ));
        let _ = tokio::fs::remove_dir_all(&extract_root).await;
        tokio::fs::create_dir_all(&extract_root).await?;

        if let Err(err) = crate::zip_extract::extract(std::io::Cursor::new(bytes), &extract_root) {
            let _ = tokio::fs::remove_dir_all(&extract_root).await;
            return Err(err.into());
        }

        // Find the icon pack directory within the extracted tree.
        fn find_pack_dir(
            root: &std::path::Path,
            name: &str,
            depth: usize,
        ) -> Option<std::path::PathBuf> {
            if depth == 0 {
                return None;
            }
            let entries = std::fs::read_dir(root).ok()?;
            for entry in entries.flatten() {
                let path = entry.path();
                let meta = std::fs::symlink_metadata(&path).ok()?;
                if meta.file_type().is_symlink() {
                    continue;
                }
                if meta.is_dir() {
                    if path.file_name().and_then(|n| n.to_str()) == Some(name) {
                        return Some(path);
                    }
                    if let Some(found) = find_pack_dir(&path, name, depth - 1) {
                        return Some(found);
                    }
                }
            }
            None
        }

        let extracted_pack = find_pack_dir(&extract_root, &id, 6)
            .ok_or_else(|| anyhow::anyhow!("extracted archive did not contain {id}"))?;

        // Basic validation before moving into place.
        if !extracted_pack.join("manifest.json").is_file() {
            return Err(anyhow::anyhow!("icon pack missing manifest.json"));
        }
        if !extracted_pack.join("icons.json").is_file() {
            return Err(anyhow::anyhow!("icon pack missing icons.json"));
        }

        // Backup existing pack dir, if present.
        let backup = temp_root.join(format!(
            "backup_iconpack_{}_{}",
            id.replace('/', "_"),
            std::process::id()
        ));
        let _ = tokio::fs::remove_dir_all(&backup).await;
        if actual.exists() {
            tokio::fs::rename(&actual, &backup).await?;
        }

        tokio::fs::create_dir_all(&packs_dir).await?;
        if let Err(e) = tokio::fs::rename(&extracted_pack, &actual).await {
            let _ = tokio::fs::remove_dir_all(&actual).await;
            if backup.exists() {
                let _ = tokio::fs::rename(&backup, &actual).await;
            }
            let _ = tokio::fs::remove_dir_all(&extract_root).await;
            return Err(e.into());
        }

        let _ = tokio::fs::remove_dir_all(&backup).await;
        let _ = tokio::fs::remove_dir_all(&extract_root).await;
        Ok(())
    }
    .await;

    result?;
    Ok(id)
}

pub async fn remove_icon_pack(id: String) -> Result<(), anyhow::Error> {
    crate::icon_packs::validate_pack_id(&id)?;
    let p = config_dir().join("icon_packs").join(id);
    tokio::fs::remove_dir_all(p).await?;
    Ok(())
}
