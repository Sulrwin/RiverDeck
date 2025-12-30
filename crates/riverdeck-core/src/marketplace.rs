//! Helpers for dealing with Elgato Marketplace deep links.

use serde::Deserialize;
use std::sync::OnceLock;
use std::sync::RwLock;

use crate::store::{NotProfile, Store};

/// Best-effort extraction of a downloadable archive URL from a marketplace deep link payload.
///
/// This is intentionally heuristic: the marketplace has used various encodings (query params,
/// JSON blobs, base64 JSON, fragments, and sometimes path-segment payloads).
pub fn extract_download_url(url: &reqwest::Url) -> Option<String> {
    fn looks_like_download_url(s: &str) -> bool {
        let s_l = s.to_lowercase();
        if !(s_l.starts_with("https://") || s_l.starts_with("http://")) {
            return false;
        }

        // Primary: direct extension match in URL.
        if s_l.contains(".streamdeckplugin")
            || s_l.contains("streamdeckplugin")
            || s_l.contains(".streamdeckiconpack")
            || s_l.contains("streamdeckiconpack")
            || s_l.ends_with(".zip")
        {
            return true;
        }

        // Secondary: some signed download URLs don't include an extension in the path, but include
        // a filename in query params like `response-content-disposition=...filename=foo.streamDeckPlugin`.
        if s_l.contains("content-disposition")
            && (s_l.contains(".streamdeckplugin")
                || s_l.contains(".streamdeckiconpack")
                || s_l.contains(".zip"))
        {
            return true;
        }

        false
    }

    fn percent_decode_best_effort(s: &str) -> Option<String> {
        fn hex(b: u8) -> Option<u8> {
            match b {
                b'0'..=b'9' => Some(b - b'0'),
                b'a'..=b'f' => Some(b - b'a' + 10),
                b'A'..=b'F' => Some(b - b'A' + 10),
                _ => None,
            }
        }

        let bytes = s.as_bytes();
        let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'%' if i + 2 < bytes.len() => {
                    let hi = hex(bytes[i + 1])?;
                    let lo = hex(bytes[i + 2])?;
                    out.push((hi << 4) | lo);
                    i += 3;
                }
                b'+' => {
                    out.push(b' ');
                    i += 1;
                }
                b => {
                    out.push(b);
                    i += 1;
                }
            }
        }
        String::from_utf8(out).ok()
    }

    fn find_in_json(v: &serde_json::Value) -> Option<String> {
        match v {
            serde_json::Value::String(s) => {
                if looks_like_download_url(s) {
                    Some(s.clone())
                } else {
                    None
                }
            }
            serde_json::Value::Array(a) => a.iter().find_map(find_in_json),
            serde_json::Value::Object(m) => m.values().find_map(find_in_json),
            _ => None,
        }
    }

    fn try_candidate(s: &str) -> Option<String> {
        if s.trim().is_empty() {
            return None;
        }
        let s = s.trim();

        if looks_like_download_url(s) {
            return Some(s.to_owned());
        }

        // Sometimes the payload is a query-string-ish blob (e.g. `downloadUrl=...&foo=bar`)
        // embedded in a fragment or path segment.
        if s.contains('=') {
            for part in s.split('&') {
                let Some((_k, v)) = part.split_once('=') else {
                    continue;
                };
                if let Some(found) = try_candidate(v) {
                    return Some(found);
                }
            }
        }

        // JSON (direct)
        if s.starts_with('{')
            && s.ends_with('}')
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(s)
            && let Some(found) = find_in_json(&val)
        {
            return Some(found);
        }

        // base64 (standard / urlsafe)
        if s.len() >= 16 && s.len() <= 16 * 1024 {
            use base64::Engine as _;
            let try_decoded = |bytes: Vec<u8>| {
                if let Ok(val) = serde_json::from_slice::<serde_json::Value>(&bytes)
                    && let Some(found) = find_in_json(&val)
                {
                    return Some(found);
                }
                if let Ok(decoded_s) = std::str::from_utf8(&bytes)
                    && looks_like_download_url(decoded_s)
                {
                    return Some(decoded_s.to_owned());
                }
                None
            };

            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(s)
                && let Some(found) = try_decoded(bytes)
            {
                return Some(found);
            }
            if let Ok(bytes) = base64::engine::general_purpose::URL_SAFE.decode(s)
                && let Some(found) = try_decoded(bytes)
            {
                return Some(found);
            }
            if let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s)
                && let Some(found) = try_decoded(bytes)
            {
                return Some(found);
            }
        }

        // percent-decode, then retry as JSON / URL / query-string-ish.
        if let Some(decoded) = percent_decode_best_effort(s) {
            let decoded = decoded.trim();
            if decoded != s
                && let Some(found) = try_candidate(decoded)
            {
                return Some(found);
            }
        }

        None
    }

    // 0) The URL itself may already be a direct download URL (common for mp-cdn links).
    if looks_like_download_url(url.as_str()) {
        return Some(url.as_str().to_owned());
    }

    // 1) Direct query params (common patterns).
    for (_k, v) in url.query_pairs() {
        if let Some(found) = try_candidate(&v) {
            return Some(found);
        }
    }

    // 2) Fragment (some payloads stick data after `#`).
    if let Some(frag) = url.fragment()
        && let Some(found) = try_candidate(frag)
    {
        return Some(found);
    }

    // 3) Path segments (some payloads embed encoded download URLs or JSON blobs in the path).
    if let Some(segs) = url.path_segments() {
        for seg in segs {
            if let Some(found) = try_candidate(seg) {
                return Some(found);
            }
        }
    }

    None
}

/// Minimal, public item metadata returned by `mp-gateway`'s `/allitems?ids=...`.
///
/// This endpoint is unauthenticated and is useful as a fallback when a deep link only contains an
/// item/variant id (e.g. `streamdeck://plugins/install/<uuid>`) but no direct download URL.
#[derive(Debug, Clone, Deserialize)]
pub struct MarketplaceItemLite {
    pub origin: Option<String>,
    pub extension: Option<String>,
    pub name: Option<String>,
    pub product_name: Option<String>,
    pub slug: Option<String>,
    pub variant_id: Option<String>,
    pub is_free: Option<bool>,
    pub needs_license: Option<bool>,
}

/// Look up marketplace item metadata by a variant/item UUID.
///
/// Returns `None` on any network/parse error (best-effort).
pub async fn lookup_item_lite(variant_id: &str) -> Option<MarketplaceItemLite> {
    let variant_id = variant_id.trim();
    if variant_id.is_empty() || variant_id.len() > 128 {
        return None;
    }

    // Note: mp-gateway expects a comma-separated list; we only pass one.
    let url = format!("https://mp-gateway.elgato.com/allitems?ids={variant_id}");
    let resp = reqwest::get(url).await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    // `reqwest` in this workspace is built without the `json` feature, so decode manually.
    let bytes = resp.bytes().await.ok()?;
    let items: Vec<MarketplaceItemLite> = serde_json::from_slice(&bytes).ok()?;
    items.into_iter().next()
}

static MARKETPLACE_ACCESS_TOKEN: OnceLock<RwLock<Option<String>>> = OnceLock::new();

fn marketplace_token_lock() -> &'static RwLock<Option<String>> {
    MARKETPLACE_ACCESS_TOKEN.get_or_init(|| RwLock::new(None))
}

/// Store the Marketplace access token.
///
/// This is a best-effort token cache used to resolve marketplace variant IDs into signed download
/// URLs. We also persist it (best-effort) for dev convenience.
pub fn set_marketplace_access_token(token: String) {
    let tok = token.trim().to_owned();
    if tok.is_empty() || tok.len() > 16 * 1024 {
        return;
    }

    // Best-effort persistence so installs can work across restarts.
    persist_marketplace_access_token(&tok);

    if let Ok(mut lock) = marketplace_token_lock().write() {
        // Avoid constant churn on periodic refresh.
        if lock.as_deref() != Some(tok.as_str()) {
            *lock = Some(tok);
            log::debug!("Marketplace access token updated");
        }
    }
}

pub fn marketplace_access_token() -> Option<String> {
    marketplace_token_lock().read().ok().and_then(|g| g.clone())
}

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
struct MarketplaceTokenDisk {
    token: String,
    /// JWT `exp` claim in seconds since epoch, if we could parse it.
    exp: Option<u64>,
    saved_at: u64,
}

impl NotProfile for MarketplaceTokenDisk {}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn jwt_exp(token: &str) -> Option<u64> {
    // JWT: header.payload.signature (payload is base64url JSON)
    let mut it = token.split('.');
    let _hdr = it.next()?;
    let payload = it.next()?;
    if payload.len() < 8 || payload.len() > 16 * 1024 {
        return None;
    }
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.as_bytes())
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("exp").and_then(|x| x.as_u64())
}

fn persist_marketplace_access_token(token: &str) {
    let token = token.trim();
    if token.is_empty() {
        return;
    }
    let disk = MarketplaceTokenDisk {
        token: token.to_owned(),
        exp: jwt_exp(token),
        saved_at: now_secs(),
    };
    let store = Store::new("marketplace_token", &crate::shared::config_dir(), disk);
    match store {
        Ok(s) => {
            if let Err(err) = s.save() {
                log::debug!("Failed to persist marketplace token: {err:#}");
            }
        }
        Err(err) => log::debug!("Failed to open marketplace token store: {err:#}"),
    }
}

/// Best-effort: load a previously saved marketplace token into memory.
///
/// Returns `true` if a non-expired token was loaded.
pub fn load_marketplace_access_token_from_disk() -> bool {
    let store = Store::new(
        "marketplace_token",
        &crate::shared::config_dir(),
        MarketplaceTokenDisk::default(),
    );
    let Ok(store) = store else {
        return false;
    };
    let tok = store.value.token.trim().to_owned();
    if tok.is_empty() {
        return false;
    }
    // If we could parse exp and it's expired (or about to), ignore it.
    let now = now_secs();
    if let Some(exp) = store.value.exp
        && exp <= now.saturating_add(30)
    {
        return false;
    }

    if let Ok(mut lock) = marketplace_token_lock().write() {
        *lock = Some(tok);
    }
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct PurchaseLink {
    pub deep_link: Option<String>,
    pub direct_link: Option<String>,
}

/// Resolve a marketplace variant/item id into a purchase link.
///
/// This requires an authenticated Marketplace token (captured from the marketplace webview).
pub async fn purchase_link(token: &str, variant_id: &str) -> Option<PurchaseLink> {
    let token = token.trim();
    let variant_id = variant_id.trim();
    if token.is_empty() || token.len() > 16 * 1024 {
        return None;
    }
    if variant_id.is_empty() || variant_id.len() > 128 {
        return None;
    }

    async fn try_fetch(token: &str, url: String, variant_id: &str) -> Option<PurchaseLink> {
        let client = reqwest::Client::new();
        let resp = client
            .get(url)
            .bearer_auth(token)
            .header("accept", "application/json")
            // Some deployments block generic HTTP clients; mimic a browser a bit.
            .header(
                "user-agent",
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) RiverDeck/1.0",
            )
            .header("origin", "https://marketplace.elgato.com")
            .header("referer", "https://marketplace.elgato.com/")
            .send()
            .await
            .ok()?;
        let status = resp.status();
        if !status.is_success() {
            log::warn!(
                "mp-gateway purchase-link failed (status={}, variant_id={})",
                status,
                variant_id
            );
            return None;
        }
        let bytes = resp.bytes().await.ok()?;
        serde_json::from_slice::<PurchaseLink>(&bytes).ok()
    }

    // Try a few variants; Marketplace frontend supports `?standard=...` (best-effort).
    let base = format!("https://mp-gateway.elgato.com/items/{variant_id}/purchase-link");
    if let Some(v) = try_fetch(token, base.clone(), variant_id).await {
        return Some(v);
    }
    if let Some(v) = try_fetch(token, format!("{base}?standard=streamdeck"), variant_id).await {
        return Some(v);
    }
    if let Some(v) = try_fetch(token, format!("{base}?standard=true"), variant_id).await {
        return Some(v);
    }
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn extracts_icon_pack_url_from_query() {
        let url = reqwest::Url::parse("streamdeck://icons/message/foo?downloadUrl=https%3A%2F%2Fexample.com%2Ftest.streamDeckIconPack").unwrap();
        let got = super::extract_download_url(&url).unwrap();
        assert_eq!(got, "https://example.com/test.streamDeckIconPack");
    }

    #[test]
    fn extracts_plugin_url_from_path_segment_percent_encoded() {
        let url = reqwest::Url::parse(
            "streamdeck://plugins/install/foo/https%3A%2F%2Fexample.com%2Ftest.streamDeckPlugin",
        )
        .unwrap();
        let got = super::extract_download_url(&url).unwrap();
        assert_eq!(got, "https://example.com/test.streamDeckPlugin");
    }

    #[test]
    fn extracts_plugin_url_from_fragment_query_string_blob() {
        let url = reqwest::Url::parse(
            "streamdeck://plugins/message/foo#downloadUrl=https%3A%2F%2Fexample.com%2Ftest.streamDeckPlugin",
        )
        .unwrap();
        let got = super::extract_download_url(&url).unwrap();
        assert_eq!(got, "https://example.com/test.streamDeckPlugin");
    }

    #[test]
    fn returns_direct_download_url_when_url_is_archive() {
        let url =
            reqwest::Url::parse("https://mp-cdn.elgato.com/versions/foo/bar/Baz.streamDeckPlugin")
                .unwrap();
        let got = super::extract_download_url(&url).unwrap();
        assert_eq!(
            got,
            "https://mp-cdn.elgato.com/versions/foo/bar/Baz.streamDeckPlugin"
        );
    }
}
