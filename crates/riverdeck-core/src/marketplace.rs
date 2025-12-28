//! Helpers for dealing with Elgato Marketplace deep links.

/// Best-effort extraction of a downloadable archive URL from a marketplace deep link payload.
///
/// This is intentionally heuristic: the marketplace has used various encodings (query params,
/// JSON blobs, base64 JSON, fragments).
pub fn extract_download_url(url: &reqwest::Url) -> Option<String> {
    fn looks_like_download_url(s: &str) -> bool {
        let s_l = s.to_lowercase();
        (s_l.starts_with("https://") || s_l.starts_with("http://"))
            && (s_l.contains(".streamdeckplugin")
                || s_l.contains("streamdeckplugin")
                || s_l.contains(".streamdeckiconpack")
                || s_l.contains("streamdeckiconpack")
                || s_l.ends_with(".zip"))
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

    // 1) Direct query params (common patterns).
    for (_k, v) in url.query_pairs() {
        if looks_like_download_url(&v) {
            return Some(v.into_owned());
        }

        // Some payloads are URL-encoded JSON or base64 JSON. Try a few best-effort decodes.
        // JSON (direct)
        if v.starts_with('{')
            && v.ends_with('}')
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(&v)
            && let Some(found) = find_in_json(&val)
        {
            return Some(found);
        }

        // base64 (standard / urlsafe)
        let v_s = v.as_ref();
        if v_s.len() >= 16 && v_s.len() <= 16 * 1024 {
            use base64::Engine as _;
            let try_decoded = |bytes: Vec<u8>| {
                if let Ok(val) = serde_json::from_slice::<serde_json::Value>(&bytes)
                    && let Some(found) = find_in_json(&val)
                {
                    return Some(found);
                }
                if let Ok(s) = std::str::from_utf8(&bytes)
                    && looks_like_download_url(s)
                {
                    return Some(s.to_owned());
                }
                None
            };

            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(v_s)
                && let Some(found) = try_decoded(bytes)
            {
                return Some(found);
            }
            if let Ok(bytes) = base64::engine::general_purpose::URL_SAFE.decode(v_s)
                && let Some(found) = try_decoded(bytes)
            {
                return Some(found);
            }
            if let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(v_s)
                && let Some(found) = try_decoded(bytes)
            {
                return Some(found);
            }
        }

        // percent-decode, then retry as JSON / URL.
        if let Some(decoded) = percent_decode_best_effort(&v) {
            if looks_like_download_url(&decoded) {
                return Some(decoded);
            }
            if decoded.starts_with('{')
                && decoded.ends_with('}')
                && let Ok(val) = serde_json::from_str::<serde_json::Value>(&decoded)
                && let Some(found) = find_in_json(&val)
            {
                return Some(found);
            }
        }
    }

    // 2) Fragment (some payloads stick data after `#`).
    if let Some(frag) = url.fragment() {
        if looks_like_download_url(frag) {
            return Some(frag.to_owned());
        }
        if let Some(decoded) = percent_decode_best_effort(frag) {
            if looks_like_download_url(&decoded) {
                return Some(decoded);
            }
            if decoded.starts_with('{')
                && decoded.ends_with('}')
                && let Ok(val) = serde_json::from_str::<serde_json::Value>(&decoded)
                && let Some(found) = find_in_json(&val)
            {
                return Some(found);
            }
        }
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
}
