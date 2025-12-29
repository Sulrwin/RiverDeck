//! Helpers for resolving OpenAction Marketplace plugin installs.
//!
//! The OpenAction Marketplace frontend at `https://marketplace.rivul.us/` is currently a thin UI
//! over a public plugin catalogue JSON hosted on GitHub Pages:
//! `https://openactionapi.github.io/plugins/catalogue.json`.
//!
//! Install actions from that marketplace trigger `opendeck://installPlugin/<pluginId>` deep links.

use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

pub const MARKETPLACE_URL: &str = "https://marketplace.rivul.us/";
pub const CATALOGUE_URL: &str = "https://openactionapi.github.io/plugins/catalogue.json";

#[derive(Debug, Clone, Deserialize)]
pub struct CatalogueEntry {
    pub name: Option<String>,
    pub author: Option<String>,
    pub repository: Option<String>,
    pub description: Option<String>,
    pub download_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedInstall {
    pub plugin_id: String,
    pub download_url: Option<String>,
    pub repository_url: Option<String>,
}

#[derive(Debug, Clone)]
struct CatalogueCache {
    fetched_at_secs: u64,
    entries: HashMap<String, CatalogueEntry>,
}

static CATALOGUE_CACHE: OnceLock<RwLock<Option<CatalogueCache>>> = OnceLock::new();

fn cache_lock() -> &'static RwLock<Option<CatalogueCache>> {
    CATALOGUE_CACHE.get_or_init(|| RwLock::new(None))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn normalize_plugin_id(id: &str) -> String {
    id.trim()
        .trim_end_matches(".sdPlugin")
        .trim_end_matches(".sdplugin")
        .to_owned()
}

fn normalize_repo_url(url: &str) -> Option<String> {
    let u = url.trim().trim_end_matches('/').to_owned();
    if u.len() > 1024 {
        return None;
    }
    if !(u.starts_with("https://") || u.starts_with("http://")) {
        return None;
    }
    Some(u)
}

fn github_owner_repo(repo_url: &str) -> Option<(String, String)> {
    // Expect URLs like: https://github.com/<owner>/<repo>(/...)?
    let u = reqwest::Url::parse(repo_url).ok()?;
    let host = u.host_str().unwrap_or_default();
    if !host.eq_ignore_ascii_case("github.com") {
        return None;
    }
    let mut segs = u.path_segments()?;
    let owner = segs.next()?.trim();
    let repo = segs.next()?.trim();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner.to_owned(), repo.to_owned()))
}

/// Fetch the OpenAction plugin catalogue (cached, best-effort).
pub async fn fetch_catalogue() -> anyhow::Result<HashMap<String, CatalogueEntry>> {
    const TTL_SECS: u64 = 10 * 60;

    let now = now_secs();
    if let Ok(guard) = cache_lock().read()
        && let Some(c) = guard.as_ref()
        && now.saturating_sub(c.fetched_at_secs) <= TTL_SECS
    {
        return Ok(c.entries.clone());
    }

    let resp = reqwest::get(CATALOGUE_URL).await?;
    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow::anyhow!(
            "OpenAction catalogue fetch failed (status={status})"
        ));
    }

    let bytes = resp.bytes().await?;
    let entries: HashMap<String, CatalogueEntry> = serde_json::from_slice(&bytes)?;

    if let Ok(mut guard) = cache_lock().write() {
        *guard = Some(CatalogueCache {
            fetched_at_secs: now,
            entries: entries.clone(),
        });
    }

    Ok(entries)
}

#[derive(Debug, Clone, Deserialize)]
struct GithubReleaseAsset {
    browser_download_url: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubRelease {
    assets: Option<Vec<GithubReleaseAsset>>,
}

fn default_user_agent() -> &'static str {
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) RiverDeck/1.0"
}

async fn github_try_latest_download(owner: &str, repo: &str, filename: &str) -> Option<String> {
    let filename = filename.trim();
    if filename.is_empty() || filename.len() > 512 {
        return None;
    }
    let url = format!("https://github.com/{owner}/{repo}/releases/latest/download/{filename}");
    let client = reqwest::Client::new();
    let resp = client
        .head(&url)
        .header("accept", "*/*")
        .header("user-agent", default_user_agent())
        .send()
        .await
        .ok()?;
    let status = resp.status();
    if status.is_success() || status.is_redirection() {
        return Some(url);
    }
    None
}

async fn github_latest_release_download(repo_url: &str, plugin_id: &str) -> Option<String> {
    let (owner, repo) = github_owner_repo(repo_url)?;

    // First: avoid GitHub API rate limits by probing conventional release asset names.
    // Many OpenAction repos publish `<plugin_id>.zip` as the bundle.
    let candidates = [
        format!("{plugin_id}.zip"),
        format!("{plugin_id}.streamDeckPlugin"),
        format!("{plugin_id}.streamDeckIconPack"),
        format!("{plugin_id}.sdPlugin.zip"),
        format!("{plugin_id}.sdPlugin"),
    ];
    for c in candidates {
        if let Some(u) = github_try_latest_download(&owner, &repo, &c).await {
            return Some(u);
        }
    }

    // Fallback: query GitHub API for the latest release assets (best-effort; can be rate-limited).
    let api = format!("https://api.github.com/repos/{owner}/{repo}/releases/latest");
    let client = reqwest::Client::new();
    let resp = client
        .get(api)
        .header("accept", "application/vnd.github+json")
        .header("user-agent", default_user_agent())
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let bytes = resp.bytes().await.ok()?;
    let rel: GithubRelease = serde_json::from_slice(&bytes).ok()?;
    let assets = rel.assets.unwrap_or_default();
    for a in assets {
        let url = a.browser_download_url.unwrap_or_default().trim().to_owned();
        if url.is_empty() {
            continue;
        }
        let name = a.name.unwrap_or_default().to_lowercase();
        let url_l = url.to_lowercase();
        if (name.ends_with(".streamdeckplugin")
            || url_l.contains(".streamdeckplugin")
            || name.ends_with(".streamdeckiconpack")
            || url_l.contains(".streamdeckiconpack")
            || name.ends_with(".zip"))
            && (url.starts_with("https://") || url.starts_with("http://"))
        {
            return Some(url);
        }
    }
    None
}

/// Resolve an OpenAction `pluginId` into an installable URL (best-effort).
///
/// - Prefer `download_url` from the catalogue.
/// - If missing, try GitHub `releases/latest` assets for a `.streamDeckPlugin`/`.zip`.
/// - Otherwise, return the `repository` URL as a hint for manual install.
pub async fn resolve_install(plugin_id: &str) -> anyhow::Result<ResolvedInstall> {
    let plugin_id = normalize_plugin_id(plugin_id);
    if plugin_id.is_empty() || plugin_id.len() > 256 {
        return Err(anyhow::anyhow!("invalid plugin id"));
    }

    let catalogue = fetch_catalogue().await?;
    let Some(entry) = catalogue.get(&plugin_id) else {
        return Err(anyhow::anyhow!("plugin not found in OpenAction catalogue"));
    };

    let repo_url = entry.repository.as_deref().and_then(normalize_repo_url);

    let url = entry
        .download_url
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_owned();
    if !url.is_empty() {
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(anyhow::anyhow!(
                "catalogue download_url has unsupported scheme"
            ));
        }
        return Ok(ResolvedInstall {
            plugin_id,
            download_url: Some(url),
            repository_url: repo_url,
        });
    }

    // Best-effort fallback: look for a release artifact on GitHub.
    if let Some(repo) = repo_url.as_deref()
        && let Some(dl) = github_latest_release_download(repo, &plugin_id).await
    {
        return Ok(ResolvedInstall {
            plugin_id,
            download_url: Some(dl),
            repository_url: repo_url,
        });
    }

    // No direct download. Surface repository URL for manual install.
    Ok(ResolvedInstall {
        plugin_id,
        download_url: None,
        repository_url: repo_url,
    })
}
