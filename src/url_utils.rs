use url::Url;

/// Resolve a (possibly relative) segment URI against a base URL.
pub fn resolve_segment_url(base_url: &Url, segment_uri: &str) -> anyhow::Result<Url> {
    if segment_uri.starts_with("http://") || segment_uri.starts_with("https://") {
        Ok(Url::parse(segment_uri)?)
    } else {
        Ok(base_url.join(segment_uri)?)
    }
}

/// Convert an upstream HTTPS URL into a proxy URL path.
pub fn proxy_url(upstream_url: &str, proxy_base: &str) -> String {
    let without_scheme = upstream_url
        .strip_prefix("https://")
        .or_else(|| upstream_url.strip_prefix("http://"))
        .unwrap_or(upstream_url);
    if proxy_base.is_empty() {
        format!("/hls2dash/{}", without_scheme)
    } else {
        format!("{}/hls2dash/{}", proxy_base.trim_end_matches('/'), without_scheme)
    }
}

/// Convert an upstream TS segment URL into a proxy init-segment URL.
pub fn proxy_init_url(upstream_url: &str, proxy_base: &str) -> String {
    let without_scheme = upstream_url
        .strip_prefix("https://")
        .or_else(|| upstream_url.strip_prefix("http://"))
        .unwrap_or(upstream_url);
    if proxy_base.is_empty() {
        format!("/hls2dash-init/{}", without_scheme)
    } else {
        format!("{}/hls2dash-init/{}", proxy_base.trim_end_matches('/'), without_scheme)
    }
}

/// Convert a media playlist URL into a playlist-based init-segment proxy URL.
/// Used for live streams so the init segment is always derived from the current playlist.
pub fn proxy_init_from_playlist_url(playlist_url: &str, proxy_base: &str) -> String {
    let without_scheme = playlist_url
        .strip_prefix("https://")
        .or_else(|| playlist_url.strip_prefix("http://"))
        .unwrap_or(playlist_url);
    if proxy_base.is_empty() {
        format!("/hls2dash-init-pl/{}", without_scheme)
    } else {
        format!("{}/hls2dash-init-pl/{}", proxy_base.trim_end_matches('/'), without_scheme)
    }
}

/// Build the upstream HTTPS URL from the captured path segment and optional query string.
pub fn build_upstream_url(path: &str, query: Option<&str>) -> String {
    let base = format!("https://{}", path.trim_start_matches('/'));
    match query {
        Some(q) if !q.is_empty() => format!("{}?{}", base, q),
        _ => base,
    }
}

/// XML-escape a string for safe embedding in MPD attributes or text.
pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proxy_url_no_base() {
        assert_eq!(
            proxy_url("https://cdn.example.com/live/master.m3u8", ""),
            "/hls2dash/cdn.example.com/live/master.m3u8"
        );
    }

    #[test]
    fn test_proxy_url_with_base() {
        assert_eq!(
            proxy_url("https://cdn.example.com/live/seg.ts", "https://myapp.com"),
            "https://myapp.com/hls2dash/cdn.example.com/live/seg.ts"
        );
    }

    #[test]
    fn test_resolve_absolute() {
        let base = Url::parse("https://cdn.example.com/live/playlist.m3u8").unwrap();
        let resolved = resolve_segment_url(&base, "https://other.example.com/seg.ts").unwrap();
        assert_eq!(resolved.as_str(), "https://other.example.com/seg.ts");
    }

    #[test]
    fn test_resolve_relative() {
        let base = Url::parse("https://cdn.example.com/live/playlist.m3u8").unwrap();
        let resolved = resolve_segment_url(&base, "seg001.ts").unwrap();
        assert_eq!(resolved.as_str(), "https://cdn.example.com/live/seg001.ts");
    }

    #[test]
    fn test_xml_escape() {
        assert_eq!(xml_escape("a&b<c>d\"e"), "a&amp;b&lt;c&gt;d&quot;e");
    }
}
