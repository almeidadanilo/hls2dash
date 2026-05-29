use crate::{
    cache::{Cache, CachedResponse},
    config::Config,
    dash::{generate_mpd, AltRepData, MpdParams, RepresentationData},
    error::AppError,
    hls::{audio_alternatives, is_fmp4, parse_playlist, subtitle_alternatives, ParsedPlaylist},
    upstream::{fetch_stream, fetch_text},
    url_utils::build_upstream_url,
};
use axum::{
    extract::{Path, RawQuery, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures::future::join_all;
use reqwest::Client;
use std::sync::Arc;
use tracing::debug;
use url::Url;

#[derive(Clone)]
pub struct AppState {
    pub http_client: Client,
    pub playlist_cache: Arc<Cache>,
    pub config: Config,
}

/// Health-check endpoint — returns the service version.
pub async fn health(State(state): State<AppState>) -> impl IntoResponse {
    (StatusCode::OK, state.config.version.clone())
}

/// Release notes endpoint — returns the contents of releasenotes.txt.
pub async fn handle_rn() -> impl IntoResponse {
    match tokio::fs::read_to_string("releasenotes.txt").await {
        Ok(content) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"))],
            content,
        )
            .into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "releasenotes.txt not found").into_response(),
    }
}

/// Dispatch handler for `/hls2dash/*path` — manifest (.m3u8) or segment passthrough.
pub async fn handle_hls2dash(
    State(state): State<AppState>,
    Path(path): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Response, AppError> {
    let upstream_url = build_upstream_url(&path, query.as_deref());

    let path_without_query = path.split('?').next().unwrap_or(&path);
    if path_without_query.ends_with(".m3u8") {
        handle_manifest(state, upstream_url).await
    } else if path_without_query.ends_with(".ts") && state.config.transmux_ts {
        handle_ts_segment(state, upstream_url).await
    } else {
        handle_segment(state, upstream_url).await
    }
}

/// Handler for `/dash/*path` — always returns a DASH MPD regardless of URL extension.
/// Accepts .mpd, .m3u8, or no extension; rewrites .mpd → .m3u8 when fetching upstream.
pub async fn handle_dash_manifest(
    State(state): State<AppState>,
    Path(path): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Response, AppError> {
    // Rewrite .mpd extension to .m3u8 for the upstream fetch.
    let hls_path = if path.ends_with(".mpd") {
        format!("{}.m3u8", &path[..path.len() - 4])
    } else {
        path
    };
    let upstream_url = build_upstream_url(&hls_path, query.as_deref());
    handle_manifest(state, upstream_url).await
}

/// Handle a manifest (.m3u8) request: fetch, parse, convert to DASH MPD.
async fn handle_manifest(state: AppState, upstream_url: String) -> Result<Response, AppError> {
    debug!(url = %upstream_url, "handling manifest");

    let (body_bytes, _ct) = fetch_text_cached(&state, &upstream_url).await?;

    match parse_playlist(&body_bytes)? {
        ParsedPlaylist::Master(master) => {
            let base_url = Url::parse(&upstream_url)
                .map_err(|e| AppError::InvalidUrl(e.to_string()))?;

            // Fetch all variant playlists in parallel.
            let variant_futures: Vec<_> = master
                .variants
                .iter()
                .map(|v| {
                    let state2 = state.clone();
                    let base = base_url.clone();
                    let variant_uri = v.uri.clone();
                    async move {
                        let abs =
                            crate::url_utils::resolve_segment_url(&base, &variant_uri)
                                .map_err(|e| AppError::InvalidUrl(e.to_string()))?;
                        let url_str = abs.to_string();
                        let (bytes, _) = fetch_text_cached(&state2, &url_str).await?;
                        match parse_playlist(&bytes)? {
                            ParsedPlaylist::Media(pl) => Ok::<_, AppError>((abs, pl)),
                            ParsedPlaylist::Master(_) => Err(AppError::ParseError(
                                "unexpected master playlist as variant".to_string(),
                            )),
                        }
                    }
                })
                .collect();

            let variant_results: Vec<Result<(Url, m3u8_rs::MediaPlaylist), AppError>> =
                join_all(variant_futures).await;

            let mut variant_playlists: Vec<(Url, m3u8_rs::MediaPlaylist)> = Vec::new();
            for r in variant_results {
                variant_playlists.push(r?);
            }

            // Collect audio alternative media entries that have a URI.
            let audio_alts: Vec<m3u8_rs::AlternativeMedia> = audio_alternatives(&master)
                .into_iter()
                .filter(|a| a.uri.is_some())
                .cloned()
                .collect();

            // Collect subtitle alternative media entries that have a URI.
            let subtitle_alts: Vec<m3u8_rs::AlternativeMedia> =
                subtitle_alternatives(&master)
                    .into_iter()
                    .filter(|a| a.uri.is_some())
                    .cloned()
                    .collect();

            // Fetch audio playlists in parallel.
            let audio_futures: Vec<_> = audio_alts
                .iter()
                .map(|alt| {
                    let state2 = state.clone();
                    let base = base_url.clone();
                    let uri = alt.uri.clone().unwrap_or_default();
                    async move {
                        let abs =
                            crate::url_utils::resolve_segment_url(&base, &uri)
                                .map_err(|e| AppError::InvalidUrl(e.to_string()))?;
                        let url_str = abs.to_string();
                        let (bytes, _) = fetch_text_cached(&state2, &url_str).await?;
                        match parse_playlist(&bytes)? {
                            ParsedPlaylist::Media(pl) => Ok::<_, AppError>(Some((abs, pl))),
                            _ => Ok(None),
                        }
                    }
                })
                .collect();

            // Fetch subtitle playlists in parallel.
            let subtitle_futures: Vec<_> = subtitle_alts
                .iter()
                .map(|alt| {
                    let state2 = state.clone();
                    let base = base_url.clone();
                    let uri = alt.uri.clone().unwrap_or_default();
                    async move {
                        let abs =
                            crate::url_utils::resolve_segment_url(&base, &uri)
                                .map_err(|e| AppError::InvalidUrl(e.to_string()))?;
                        let url_str = abs.to_string();
                        let (bytes, _) = fetch_text_cached(&state2, &url_str).await?;
                        match parse_playlist(&bytes)? {
                            ParsedPlaylist::Media(pl) => Ok::<_, AppError>(Some((abs, pl))),
                            _ => Ok(None),
                        }
                    }
                })
                .collect();

            let audio_results: Vec<Result<Option<(Url, m3u8_rs::MediaPlaylist)>, AppError>> =
                join_all(audio_futures).await;
            let subtitle_results: Vec<
                Result<Option<(Url, m3u8_rs::MediaPlaylist)>, AppError>,
            > = join_all(subtitle_futures).await;

            let mut audio_playlists: Vec<Option<(Url, m3u8_rs::MediaPlaylist)>> = Vec::new();
            for r in audio_results {
                audio_playlists.push(r?);
            }
            let mut subtitle_playlists: Vec<Option<(Url, m3u8_rs::MediaPlaylist)>> =
                Vec::new();
            for r in subtitle_results {
                subtitle_playlists.push(r?);
            }

            // Build RepresentationData for video streams.
            let video_reps: Vec<RepresentationData<'_>> = master
                .variants
                .iter()
                .enumerate()
                .filter_map(|(i, variant)| {
                    variant_playlists
                        .get(i)
                        .map(|(url, pl)| RepresentationData {
                            id: format!("v{}", i + 1),
                            variant,
                            media_playlist: pl,
                            playlist_url: url.clone(),
                            is_fmp4: is_fmp4(pl),
                        })
                })
                .collect();

            // Build AltRepData for audio.
            let audio_reps: Vec<AltRepData<'_>> = audio_alts
                .iter()
                .enumerate()
                .map(|(i, alt)| {
                    let pl_data = audio_playlists
                        .get(i)
                        .and_then(|o| o.as_ref())
                        .map(|(u, p)| (u, p));
                    AltRepData {
                        id: format!("a{}", i + 1),
                        alt,
                        media_playlist: pl_data.map(|(_, p)| p),
                        playlist_url: pl_data.map(|(u, _)| u.clone()),
                        is_fmp4: pl_data.map(|(_, p)| is_fmp4(p)).unwrap_or(false),
                    }
                })
                .collect();

            // Build AltRepData for subtitles.
            let subtitle_reps: Vec<AltRepData<'_>> = subtitle_alts
                .iter()
                .enumerate()
                .map(|(i, alt)| {
                    let pl_data = subtitle_playlists
                        .get(i)
                        .and_then(|o| o.as_ref())
                        .map(|(u, p)| (u, p));
                    AltRepData {
                        id: format!("s{}", i + 1),
                        alt,
                        media_playlist: pl_data.map(|(_, p)| p),
                        playlist_url: pl_data.map(|(u, _)| u.clone()),
                        is_fmp4: pl_data.map(|(_, p)| is_fmp4(p)).unwrap_or(false),
                    }
                })
                .collect();

            let params = MpdParams {
                video_reps,
                audio_reps,
                subtitle_reps,
                proxy_base: &state.config.proxy_base,
                transmux_ts: state.config.transmux_ts,
            };

            let mpd = generate_mpd(&params);
            mpd_response(mpd)
        }

        ParsedPlaylist::Media(media) => {
            // Direct media playlist request (no master) — wrap in a single-stream MPD.
            let base_url = Url::parse(&upstream_url)
                .map_err(|e| AppError::InvalidUrl(e.to_string()))?;

            let fmp4 = is_fmp4(&media);

            // Build a minimal VariantStream for the single stream.
            let dummy_variant = make_dummy_variant(upstream_url.clone(), 0);

            let video_reps = vec![RepresentationData {
                id: "v1".to_string(),
                variant: &dummy_variant,
                media_playlist: &media,
                playlist_url: base_url,
                is_fmp4: fmp4,
            }];

            let params = MpdParams {
                video_reps,
                audio_reps: vec![],
                subtitle_reps: vec![],
                proxy_base: &state.config.proxy_base,
                transmux_ts: state.config.transmux_ts,
            };

            let mpd = generate_mpd(&params);
            mpd_response(mpd)
        }
    }
}

/// Handle a segment/key request: stream bytes from upstream without buffering.
async fn handle_segment(state: AppState, upstream_url: String) -> Result<Response, AppError> {
    debug!(url = %upstream_url, "handling segment/key passthrough");

    let (body, content_type, content_length) =
        fetch_stream(&state.http_client, &upstream_url).await?;

    let mut headers = HeaderMap::new();
    if let Some(ct) = content_type {
        if let Ok(hv) = HeaderValue::from_str(&ct) {
            headers.insert(header::CONTENT_TYPE, hv);
        }
    }
    if let Some(len) = content_length {
        if let Ok(hv) = HeaderValue::from_str(&len.to_string()) {
            headers.insert(header::CONTENT_LENGTH, hv);
        }
    }

    Ok((StatusCode::OK, headers, body).into_response())
}

/// Fetch a playlist text from cache, or upstream on cache miss.
async fn fetch_text_cached(
    state: &AppState,
    url: &str,
) -> Result<(Bytes, String), AppError> {
    let key = url.to_string();
    let client = state.http_client.clone();
    let url_owned = url.to_string();

    let result = state
        .playlist_cache
        .get_or_fetch(key, move || async move {
            let (bytes, ct) = fetch_text(&client, &url_owned)
                .await
                .map_err(anyhow::Error::from)?;
            Ok(CachedResponse {
                body: bytes,
                content_type: ct,
            })
        })
        .await
        .map_err(|e| AppError::ParseError(e.to_string()))?;

    Ok((result.body, result.content_type))
}

/// Wrap an MPD string in the correct HTTP response.
fn mpd_response(mpd: String) -> Result<Response, AppError> {
    Ok((
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/dash+xml"),
        )],
        mpd,
    )
        .into_response())
}

/// Handle a `.ts` segment request with transmuxing: fetch TS, pipe through ffmpeg, return fMP4 media portion.
async fn handle_ts_segment(state: AppState, upstream_url: String) -> Result<Response, AppError> {
    debug!(url = %upstream_url, "handling TS segment with transmux");

    let (ts_bytes, _) = fetch_text(&state.http_client, &upstream_url).await?;
    let fmp4 = crate::transmux::transmux_ts(ts_bytes)
        .await
        .map_err(|e| AppError::ParseError(e.to_string()))?;
    let media = crate::transmux::extract_media(&fmp4)
        .ok_or_else(|| AppError::ParseError("no moof box in transmuxed output".into()))?;
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, HeaderValue::from_static("video/mp4"))],
        media,
    )
        .into_response())
}

/// Handler for `/hls2dash-init/*path` — returns the init segment (ftyp+moov) for a TS segment URL.
pub async fn handle_ts_init(
    State(state): State<AppState>,
    Path(path): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Response, AppError> {
    let upstream_url = build_upstream_url(&path, query.as_deref());
    debug!(url = %upstream_url, "handling TS init segment");

    let (ts_bytes, _) = fetch_text(&state.http_client, &upstream_url).await?;
    let fmp4 = crate::transmux::transmux_ts(ts_bytes)
        .await
        .map_err(|e| AppError::ParseError(e.to_string()))?;
    let init = crate::transmux::extract_init(&fmp4)
        .ok_or_else(|| AppError::ParseError("no moov box found in transmuxed output".into()))?;
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, HeaderValue::from_static("video/mp4"))],
        init,
    )
        .into_response())
}

/// Handler for `/hls2dash-init-pl/*path` — passes the media playlist URL directly to
/// FFmpeg so its HLS demuxer fetches segments and handles AES-128 decryption automatically.
/// Returns the ftyp+moov init segment.
pub async fn handle_ts_init_from_playlist(
    State(_state): State<AppState>,
    Path(path): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Response, AppError> {
    let playlist_url = build_upstream_url(&path, query.as_deref());
    debug!(url = %playlist_url, "handling TS init from playlist via ffmpeg HLS demuxer");

    let fmp4 = crate::transmux::transmux_ts_from_url(&playlist_url)
        .await
        .map_err(|e| AppError::ParseError(e.to_string()))?;
    let init = crate::transmux::extract_init(&fmp4)
        .ok_or_else(|| AppError::ParseError("no moov box found in transmuxed output".into()))?;

    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, HeaderValue::from_static("video/mp4"))],
        init,
    )
        .into_response())
}

/// Handler for `/hls2dash-ts-pl/*path` — serves a single transmuxed TS segment from
/// an HLS playlist. Supports AES-128 encrypted streams because FFmpeg's HLS demuxer
/// fetches and decrypts automatically. `_idx` and `_dur` params are stripped before
/// the remaining query is used to reconstruct the upstream playlist URL.
pub async fn handle_ts_segment_from_playlist(
    State(_state): State<AppState>,
    Path(path): Path<String>,
    RawQuery(raw_query): RawQuery,
) -> Result<Response, AppError> {
    // Split _idx and _dur out of the query string; the rest belongs to the playlist URL.
    let mut seg_idx: usize = 0;
    let mut target_dur_ms: u64 = 6000;
    let mut other: Vec<&str> = Vec::new();

    if let Some(q) = raw_query.as_deref() {
        for part in q.split('&') {
            if let Some(v) = part.strip_prefix("_idx=") {
                seg_idx = v.parse().unwrap_or(0);
            } else if let Some(v) = part.strip_prefix("_dur=") {
                target_dur_ms = v.parse().unwrap_or(6000);
            } else {
                other.push(part);
            }
        }
    }

    let playlist_query = if other.is_empty() { None } else { Some(other.join("&")) };
    let playlist_url = build_upstream_url(&path, playlist_query.as_deref());
    let seek_secs = (seg_idx as f64) * (target_dur_ms as f64 / 1000.0);
    let duration_secs = target_dur_ms as f64 / 1000.0;

    debug!(url = %playlist_url, idx = seg_idx, seek = seek_secs, "TS segment from playlist");

    let fmp4 = crate::transmux::transmux_ts_from_playlist_at(&playlist_url, seek_secs, duration_secs)
        .await
        .map_err(|e| AppError::ParseError(e.to_string()))?;
    let media = crate::transmux::extract_media(&fmp4)
        .ok_or_else(|| AppError::ParseError("no moof box in transmuxed segment output".into()))?;

    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, HeaderValue::from_static("video/mp4"))],
        media,
    )
        .into_response())
}

/// Build a minimal `VariantStream` for use when there is no master playlist.
fn make_dummy_variant(uri: String, bandwidth: u64) -> m3u8_rs::VariantStream {
    m3u8_rs::VariantStream {
        uri,
        bandwidth,
        average_bandwidth: None,
        codecs: None,
        resolution: None,
        frame_rate: None,
        hdcp_level: None,
        audio: None,
        video: None,
        subtitles: None,
        closed_captions: None,
        other_attributes: None,
        is_i_frame: false,
    }
}
