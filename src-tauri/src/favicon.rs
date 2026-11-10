//! Best-effort favicon discovery for apps added by URL.
//!
//! A custom app has no catalog entry, so it starts with no icon. Rather than
//! leaving a blank tile, ask the site itself: fetch the page, read the
//! `<link rel="icon">` declarations it ships (apple-touch-icon and larger
//! `sizes` win), then fall back to the conventional `/favicon.ico`.
//!
//! The first image that decodes as a real picture comes back as a data URI, so
//! the icon travels with the app entry in config.json - no asset server, no
//! side files to lose track of, and it keeps working offline once saved.

use base64::Engine as _;
use std::sync::OnceLock;
use std::time::Duration;

/// Browser-ish UA: many sites answer a bare HTTP client with a bot page that
/// carries no icon links at all.
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// Stored as base64 inside config.json, so an icon has to stay small.
const MAX_ICON_BYTES: usize = 128 * 1024;
/// Pages bigger than this are almost never real entry pages worth parsing.
const MAX_HTML_BYTES: usize = 512 * 1024;

/// One client for the whole process: reuses connections and the TLS setup, so a
/// background icon fetch costs no handshake beyond the first.
fn client() -> Result<&'static reqwest::Client, String> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .user_agent(UA)
                // Fail fast on a dead host: this runs in the background, but a
                // long connect wait still holds a slot for no benefit.
                .connect_timeout(Duration::from_secs(4))
                .timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::limited(6))
                .build()
                .map_err(|e| format!("could not start HTTP client: {e}"))
        })
        .as_ref()
        .map_err(|e| e.clone())
}

/// Read at most `cap` bytes. A `Content-Length` is a hint, not a promise, so the
/// body is streamed and abandoned once it exceeds the cap instead of buffering
/// a multi-megabyte page to discover it is too big.
async fn read_capped(mut resp: reqwest::Response, cap: usize) -> Result<Vec<u8>, String> {
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        if buf.len() + chunk.len() > cap {
            return Err(format!("response exceeds {cap} bytes"));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Accept what people actually type: `example.com`, `example.com/inbox`.
fn absolute(input: &str) -> Result<url::Url, String> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err("Enter a URL first.".into());
    }
    let with_scheme = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("https://{raw}")
    };
    let mut parsed = url::Url::parse(&with_scheme).map_err(|_| format!("Not a valid URL: {raw}"))?;
    if parsed.host_str().is_none() {
        return Err(format!("Not a valid URL: {raw}"));
    }
    // Only http(s) can be fetched and only http(s) makes sense as an app.
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err("Only http and https URLs can be inspected.".into());
    }
    parsed.set_fragment(None);
    Ok(parsed)
}

/// Identify the image format from the bytes themselves. The `Content-Type` of
/// a favicon is wrong often enough (served as text/plain, or octet-stream) that
/// sniffing is the more reliable signal.
fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes.starts_with(b"BM") && bytes.len() > 14 {
        return Some("image/bmp");
    }
    if bytes.starts_with(&[0x00, 0x00, 0x01, 0x00]) || bytes.starts_with(&[0x00, 0x00, 0x02, 0x00]) {
        return Some("image/x-icon");
    }
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]).to_ascii_lowercase();
    if head.contains("<svg") || (head.contains("<?xml") && head.contains("svg")) {
        return Some("image/svg+xml");
    }
    None
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

/// Read one attribute out of a raw HTML tag. Deliberately tiny instead of a
/// parser dependency: `<link>` tags are flat and we only need three attributes.
fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0usize;
    while let Some(offset) = lower[from..].find(name) {
        let at = from + offset;
        from = at + name.len();
        // Must start a token: either the tag start or preceded by whitespace.
        if at > 0 && !lower.as_bytes()[at - 1].is_ascii_whitespace() {
            continue;
        }
        let after = lower[at + name.len()..].trim_start();
        let Some(rest) = after.strip_prefix('=') else {
            continue;
        };
        let value = rest.trim_start();
        let Some(quote) = value.chars().next() else {
            continue;
        };
        let raw = if quote == '"' || quote == '\'' {
            let body = &value[1..];
            match body.find(quote) {
                Some(end) => &body[..end],
                None => continue,
            }
        } else {
            let end = value
                .find(|c: char| c.is_ascii_whitespace() || c == '>' || c == '/')
                .unwrap_or(value.len());
            &value[..end]
        };
        return Some(decode_entities(raw));
    }
    None
}

/// Bigger declared icon wins: `sizes="180x180"` beats `sizes="16x16"`.
fn sizes_score(sizes: &str) -> i32 {
    let mut best = 0i32;
    for token in sizes.split_whitespace() {
        if let Some((w, _h)) = token.split_once('x')
            && let Ok(n) = w.trim().parse::<i32>()
        {
            best = best.max(n);
        }
    }
    match best {
        0 => 0,
        n if n >= 180 => 30,
        n if n >= 96 => 24,
        n if n >= 48 => 16,
        n if n >= 32 => 10,
        _ => 4,
    }
}

/// Every `<link rel="...icon...">` in the document, best candidate first.
fn icon_candidates(html: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let mut scored: Vec<(String, i32)> = Vec::new();
    let mut cursor = 0usize;
    while let Some(offset) = lower[cursor..].find("<link") {
        let start = cursor + offset;
        let Some(close) = lower[start..].find('>') else {
            break;
        };
        let end = start + close;
        let tag = &html[start..end];
        cursor = end + 1;
        let Some(rel) = attr(tag, "rel") else {
            continue;
        };
        let rel = rel.to_ascii_lowercase();
        if !rel.contains("icon") {
            continue;
        }
        let Some(href) = attr(tag, "href") else {
            continue;
        };
        if href.is_empty() || href.starts_with("data:") {
            continue;
        }
        let mut score = if rel.contains("apple-touch-icon") {
            40
        } else if rel.contains("mask-icon") {
            4
        } else {
            20
        };
        score += sizes_score(&attr(tag, "sizes").unwrap_or_default());
        scored.push((href, score));
    }
    scored.sort_by(|a, b| b.1.cmp(&a.1));
    scored.into_iter().map(|(href, _)| href).collect()
}

async fn download(
    client: &reqwest::Client,
    url: &str,
) -> Result<String, String> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("{url}: HTTP {}", resp.status().as_u16()));
    }
    let bytes = read_capped(resp, MAX_ICON_BYTES)
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    if bytes.is_empty() {
        return Err(format!("{url}: empty response"));
    }
    let mime = sniff_mime(&bytes).ok_or_else(|| format!("{url}: not an image"))?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("data:{mime};base64,{b64}"))
}

/// Find an icon for `url` and return it as a `data:` URI.
///
/// Errors are ordinary outcomes here (offline, no icon, blocked by the site),
/// so the caller shows them inline and never blocks saving the app.
#[tauri::command]
pub async fn fetch_favicon(url: String) -> Result<String, String> {
    let base = absolute(&url)?;
    let client = client()?;

    let mut tried: Vec<String> = Vec::new();

    // 1. What the page itself declares.
    if let Ok(resp) = client.get(base.as_str()).send().await
        && resp.status().is_success()
        && let Ok(body) = read_capped(resp, MAX_HTML_BYTES).await
    {
        let html = String::from_utf8_lossy(&body);
        for href in icon_candidates(&html) {
            let Ok(resolved) = base.join(&href) else {
                continue;
            };
            if resolved.scheme() != "http" && resolved.scheme() != "https" {
                continue;
            }
            let candidate = resolved.to_string();
            if tried.contains(&candidate) {
                continue;
            }
            tried.push(candidate.clone());
            if let Ok(icon) = download(client, &candidate).await {
                return Ok(icon);
            }
        }
    }

    // 2. The convention every browser also tries.
    if let Ok(root) = base.join("/favicon.ico") {
        let candidate = root.to_string();
        if !tried.contains(&candidate) {
            if let Ok(icon) = download(client, &candidate).await {
                return Ok(icon);
            }
            tried.push(candidate);
        }
    }

    Err(format!(
        "No usable favicon found for {}. Paste an image URL instead.",
        base.host_str().unwrap_or(&url)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_bare_hosts_and_paths() {
        assert_eq!(absolute("example.com").unwrap().as_str(), "https://example.com/");
        assert_eq!(
            absolute("example.com/inbox").unwrap().as_str(),
            "https://example.com/inbox"
        );
        assert_eq!(
            absolute("http://example.com").unwrap().as_str(),
            "http://example.com/"
        );
        assert!(absolute("").is_err());
        assert!(absolute("ftp://example.com").is_err());
    }

    #[test]
    fn reads_attributes_in_any_order_and_quoting() {
        let tag = r#"<link href="/a.png" rel='icon' sizes="32x32">"#;
        assert_eq!(attr(tag, "rel").as_deref(), Some("icon"));
        assert_eq!(attr(tag, "href").as_deref(), Some("/a.png"));
        assert_eq!(attr(tag, "sizes").as_deref(), Some("32x32"));
        assert_eq!(attr(tag, "type"), None);
    }

    #[test]
    fn prefers_apple_touch_and_larger_sizes() {
        let html = r#"
            <link rel="shortcut icon" href="/small.ico" sizes="16x16">
            <link rel="icon" href="/mid.png" sizes="32x32">
            <link rel="apple-touch-icon" href="/touch.png" sizes="180x180">
            <link rel="stylesheet" href="/app.css">
        "#;
        let found = icon_candidates(html);
        assert_eq!(found.first().map(String::as_str), Some("/touch.png"));
        assert!(found.contains(&"/small.ico".to_string()));
        assert!(!found.contains(&"/app.css".to_string()));
    }

    #[test]
    fn ignores_inline_data_uris() {
        let html = r#"<link rel="icon" href="data:image/png;base64,AAAA">"#;
        assert!(icon_candidates(html).is_empty());
    }

    #[test]
    fn sniffs_real_image_headers() {
        assert_eq!(sniff_mime(&[0x89, b'P', b'N', b'G', 13, 10, 26, 10]), Some("image/png"));
        assert_eq!(sniff_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff_mime(b"GIF89a......"), Some("image/gif"));
        assert_eq!(sniff_mime(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff_mime(b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>"), Some("image/svg+xml"));
        assert_eq!(sniff_mime(b"<html><body>nope</body></html>"), None);
        assert_eq!(sniff_mime(&[]), None);
    }

    /// Live network check, run explicitly: cargo test --release -- --ignored
    #[test]
    #[ignore]
    fn fetches_real_favicons() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        // A spread of strategies: <link rel=icon>, apple-touch-icon only,
        // bare /favicon.ico, and a path rather than a host root.
        for (host, label) in [
            ("github.com", "link rel=icon"),
            ("news.ycombinator.com", "apple-touch-icon or ico"),
            ("example.com", "no icon links"),
            ("wikipedia.org/wiki/Rust_(programming_language)", "path URL"),
        ] {
            match rt.block_on(fetch_favicon(host.into())) {
                Ok(icon) => {
                    assert!(icon.starts_with("data:image/"), "{host}: not an image");
                    assert!(icon.len() > 200, "{host}: payload looks empty");
                    println!("{host} ({label}) -> {} bytes", icon.len());
                }
                // Some sites legitimately refuse; that must be a clean error, not a panic.
                Err(e) => println!("{host} ({label}) -> no icon: {e}"),
            }
        }
    }
}
