use anyhow::Context as _;

use semver::Version;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageKind {
    Deb,
    Rpm,
}

#[derive(Debug, Clone)]
pub struct UpdateAsset {
    pub kind: PackageKind,
    pub filename: String,
    pub url: String,
    pub size_bytes: Option<u64>,
    pub sha256_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LatestRelease {
    pub tag: String,
    pub body: String,
    pub remote_version: Version,
    pub asset: Option<UpdateAsset>,
}

#[derive(Debug, Clone)]
pub struct DownloadProgress {
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
}

fn arch_patterns() -> anyhow::Result<(&'static str, &'static str)> {
    // Keep parity with `install_riverdeck.sh` asset selection.
    match std::env::consts::ARCH {
        "x86_64" => Ok(("amd64", "x86_64")),
        "aarch64" => Ok(("arm64", "aarch64")),
        other => Err(anyhow::anyhow!("unsupported architecture: {other}")),
    }
}

fn command_exists(cmd: &str) -> bool {
    std::process::Command::new(cmd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

pub fn preferred_package_kind() -> Option<PackageKind> {
    if command_exists("dpkg") {
        Some(PackageKind::Deb)
    } else if command_exists("rpm") {
        Some(PackageKind::Rpm)
    } else {
        None
    }
}

pub async fn fetch_latest_release(
    current: Option<Version>,
) -> anyhow::Result<Option<LatestRelease>> {
    let res = reqwest::Client::new()
        .get("https://api.github.com/repos/sulrwin/RiverDeck/releases/latest")
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "RiverDeck")
        .send()
        .await
        .context("failed to request GitHub latest release")?;

    let json = res
        .error_for_status()
        .context("GitHub latest release request failed")?
        .json::<serde_json::Value>()
        .await
        .context("failed to parse GitHub latest release JSON")?;

    let tag_name = json
        .get("tag_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_owned();
    if tag_name.is_empty() {
        return Ok(None);
    }

    let body = json
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_owned();

    let remote_str = tag_name.strip_prefix('v').unwrap_or(tag_name.as_str());
    let remote_version = Version::parse(remote_str)
        .with_context(|| format!("failed to parse remote version from tag {tag_name}"))?;

    if let Some(cur) = current
        && cur >= remote_version
    {
        return Ok(None);
    }

    let preferred = preferred_package_kind();
    let asset = match preferred {
        Some(PackageKind::Deb) => select_asset(&json, PackageKind::Deb).or_else(|| {
            // If the system prefers deb but no deb asset is present, fall back to rpm.
            select_asset(&json, PackageKind::Rpm)
        }),
        Some(PackageKind::Rpm) => {
            select_asset(&json, PackageKind::Rpm).or_else(|| select_asset(&json, PackageKind::Deb))
        }
        None => None,
    };

    Ok(Some(LatestRelease {
        tag: tag_name,
        body,
        remote_version,
        asset,
    }))
}

fn select_asset(json: &serde_json::Value, kind: PackageKind) -> Option<UpdateAsset> {
    let (arch_deb, arch_rpm) = arch_patterns().ok()?;
    let (arch_pat, ext) = match kind {
        PackageKind::Deb => (arch_deb, "deb"),
        PackageKind::Rpm => (arch_rpm, "rpm"),
    };

    let assets = json.get("assets").and_then(|v| v.as_array())?;

    for a in assets {
        let url = a.get("browser_download_url")?.as_str()?.to_owned();
        let filename = a.get("name")?.as_str()?.to_owned();
        let size_bytes = a.get("size").and_then(|v| v.as_u64());

        // Keep parity with the install script: match `...<arch>.<ext>`.
        let needle = format!("{arch_pat}.{ext}");
        if filename.contains(&needle) || url.contains(&needle) {
            let sha256_url = find_sha256_url_for_asset(assets, &filename);
            return Some(UpdateAsset {
                kind,
                filename,
                url,
                size_bytes,
                sha256_url,
            });
        }
    }
    None
}

fn find_sha256_url_for_asset(assets: &[serde_json::Value], filename: &str) -> Option<String> {
    let want = format!("{filename}.sha256");
    for a in assets {
        let name = a.get("name").and_then(|v| v.as_str())?;
        if name == want {
            return a
                .get("browser_download_url")
                .and_then(|v| v.as_str())
                .map(|s| s.to_owned());
        }
    }
    None
}

pub async fn fetch_expected_sha256(asset: &UpdateAsset) -> anyhow::Result<Option<String>> {
    let Some(url) = asset.sha256_url.as_ref() else {
        return Ok(None);
    };

    let text = reqwest::Client::new()
        .get(url)
        .header("Accept", "application/octet-stream")
        .header("User-Agent", "RiverDeck")
        .send()
        .await
        .with_context(|| format!("failed to fetch checksum for {}", asset.filename))?
        .error_for_status()
        .with_context(|| format!("checksum request failed for {}", asset.filename))?
        .text()
        .await
        .context("failed to read checksum body")?;

    // Common formats:
    // - "<hash>  <filename>"
    // - "<hash>"
    let first = text.split_whitespace().next().unwrap_or("").trim();
    if first.len() != 64 || !first.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(anyhow::anyhow!(
            "unexpected sha256 format for {}",
            asset.filename
        ));
    }
    Ok(Some(first.to_lowercase()))
}

pub async fn verify_sha256(path: &Path, expected_hex: &str) -> anyhow::Result<()> {
    use sha2::Digest as _;
    use tokio::io::AsyncReadExt as _;

    let expected = expected_hex.trim().to_lowercase();
    if expected.len() != 64 || !expected.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(anyhow::anyhow!("invalid expected sha256"));
    }

    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; 1024 * 128];
    loop {
        let n = file.read(&mut buf).await.context("failed to read file")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let actual = hasher.finalize();
    let actual_hex = hex::encode(actual);
    if actual_hex != expected {
        return Err(anyhow::anyhow!(
            "sha256 mismatch for {} (expected {}, got {})",
            path.display(),
            expected,
            actual_hex
        ));
    }
    Ok(())
}

pub async fn download_asset_to(
    asset: &UpdateAsset,
    dest: &Path,
    mut on_progress: impl FnMut(DownloadProgress) + Send + 'static,
) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let res = client
        .get(&asset.url)
        .header("Accept", "application/octet-stream")
        .header("User-Agent", "RiverDeck")
        .send()
        .await
        .with_context(|| format!("failed to request asset {}", asset.filename))?
        .error_for_status()
        .with_context(|| format!("asset download failed for {}", asset.filename))?;

    let total = res.content_length().or(asset.size_bytes);

    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create update dir {}", parent.display()))?;
    }

    let tmp = dest.with_extension(format!(
        "{}.part",
        dest.extension().and_then(|s| s.to_str()).unwrap_or("bin")
    ));
    let mut file = tokio::fs::File::create(&tmp)
        .await
        .with_context(|| format!("failed to create {}", tmp.display()))?;

    use futures_util::StreamExt as _;
    use tokio::io::AsyncWriteExt as _;

    let mut downloaded: u64 = 0;
    let mut stream = res.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("download stream error")?;
        file.write_all(&chunk)
            .await
            .context("failed to write chunk")?;
        downloaded = downloaded.saturating_add(chunk.len() as u64);
        on_progress(DownloadProgress {
            downloaded_bytes: downloaded,
            total_bytes: total,
        });
    }

    file.flush().await.ok();
    drop(file);

    tokio::fs::rename(&tmp, dest)
        .await
        .with_context(|| format!("failed to finalize download to {}", dest.display()))?;

    Ok(())
}

pub fn default_update_download_path(tag: &str, kind: PackageKind) -> PathBuf {
    let ext = match kind {
        PackageKind::Deb => "deb",
        PackageKind::Rpm => "rpm",
    };
    riverdeck_core::shared::data_dir()
        .join("updates")
        .join(format!("riverdeck_{tag}.{ext}"))
}

pub async fn install_package_with_pkexec(path: &Path, kind: PackageKind) -> anyhow::Result<()> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (path, kind);
        return Err(anyhow::anyhow!("self-install is only supported on Linux"));
    }

    #[cfg(target_os = "linux")]
    {
        if !command_exists("pkexec") {
            let cmd = match kind {
                PackageKind::Deb => format!("sudo dpkg -i {}", path.display()),
                PackageKind::Rpm => format!("sudo rpm -Uvh --replacepkgs {}", path.display()),
            };
            return Err(anyhow::anyhow!(
                "pkexec not found; cannot elevate privileges automatically. Try: {cmd}"
            ));
        }

        let mut cmd = tokio::process::Command::new("pkexec");
        match kind {
            PackageKind::Deb => {
                cmd.arg("dpkg").arg("-i").arg(path);
            }
            PackageKind::Rpm => {
                cmd.arg("rpm").arg("-Uvh").arg("--replacepkgs").arg(path);
            }
        }
        // Best-effort: capture output for error messages, but don't spam logs on success.
        let out = cmd.output().await.with_context(|| {
            format!("failed to run installer via pkexec for {}", path.display())
        })?;
        if !out.status.success() {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(anyhow::anyhow!(
                "installer failed ({}):\n{}\n{}",
                out.status,
                stdout.trim(),
                stderr.trim()
            ));
        }
        Ok(())
    }
}
