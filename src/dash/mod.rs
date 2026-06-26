use crate::url_utils::{proxy_init_from_playlist_url, proxy_init_url, proxy_ts_pl_url, proxy_url, resolve_segment_url, xml_escape};
use chrono::Utc;
use m3u8_rs::{AlternativeMedia, MediaPlaylist, VariantStream};
use url::Url;

/// All data needed to generate one Representation.
pub struct RepresentationData<'a> {
    pub id: String,
    pub variant: &'a VariantStream,
    pub media_playlist: &'a MediaPlaylist,
    pub playlist_url: Url,
    pub is_fmp4: bool,
}

/// All data needed to generate one audio/subtitle AdaptationSet.
pub struct AltRepData<'a> {
    pub id: String,
    pub alt: &'a AlternativeMedia,
    pub media_playlist: Option<&'a MediaPlaylist>,
    pub playlist_url: Option<Url>,
    pub is_fmp4: bool,
}

/// Top-level parameters for MPD generation.
pub struct MpdParams<'a> {
    pub video_reps: Vec<RepresentationData<'a>>,
    pub audio_reps: Vec<AltRepData<'a>>,
    pub subtitle_reps: Vec<AltRepData<'a>>,
    pub proxy_base: &'a str,
    pub transmux_ts: bool,
}

/// Generate a complete DASH MPD XML string.
pub fn generate_mpd(params: &MpdParams<'_>) -> String {
    // Determine VOD vs live from the first available media playlist.
    let reference_playlist = params
        .video_reps
        .first()
        .map(|r| r.media_playlist)
        .or_else(|| {
            params
                .audio_reps
                .first()
                .and_then(|r| r.media_playlist)
        });

    let is_vod = reference_playlist.map(|p| p.end_list).unwrap_or(true);

    let target_duration = reference_playlist
        .map(|p| p.target_duration as f64)
        .unwrap_or(6.0);

    let total_secs: f64 = if is_vod {
        reference_playlist
            .map(|p| p.segments.iter().map(|s| s.duration as f64).sum())
            .unwrap_or(0.0)
    } else {
        0.0
    };

    let media_seq = reference_playlist
        .map(|p| p.media_sequence)
        .unwrap_or(0);

    let n_segs = reference_playlist
        .map(|p| p.segments.len())
        .unwrap_or(0);

    // Build MPD timing attributes.
    let timing_attrs = if is_vod {
        format!(
            r#"type="static" mediaPresentationDuration="PT{:.3}S""#,
            total_secs
        )
    } else {
        // Anchor availabilityStartTime so that our SegmentList (indices 0..n_segs-1)
        // aligns with the live window. Index 0 = oldest segment = n_segs * target_dur ago.
        // Using media_seq here would place index 0 thousands of segments in the past for
        // long-running streams, causing players to look for segment indices beyond our list.
        let _ = media_seq; // retained for potential future use
        let shift_secs = (n_segs as f64) * target_duration;
        let avail_start = Utc::now() - chrono::Duration::milliseconds((shift_secs * 1000.0) as i64);
        let avail_start_str = avail_start.format("%Y-%m-%dT%H:%M:%SZ");
        let tsb_depth = (n_segs as f64) * target_duration;
        let suggested_delay = target_duration * 3.0;
        format!(
            r#"type="dynamic" availabilityStartTime="{}" minimumUpdatePeriod="PT{:.1}S" timeShiftBufferDepth="PT{:.1}S" suggestedPresentationDelay="PT{:.1}S""#,
            avail_start_str, target_duration, tsb_depth, suggested_delay
        )
    };

    let mut adaptation_sets = String::new();
    let mut as_id = 1usize;

    // Video AdaptationSet
    if !params.video_reps.is_empty() {
        adaptation_sets.push_str(&generate_video_adaptation_set(
            as_id,
            &params.video_reps,
            params.proxy_base,
            params.transmux_ts,
        ));
        as_id += 1;
    }

    // Audio AdaptationSets from EXT-X-MEDIA alternates.
    for audio in &params.audio_reps {
        adaptation_sets.push_str(&generate_audio_adaptation_set(
            as_id,
            audio,
            params.proxy_base,
            params.transmux_ts,
        ));
        as_id += 1;
    }

    let _ = as_id;

    // Subtitle AdaptationSets
    for sub in &params.subtitle_reps {
        adaptation_sets.push_str(&generate_subtitle_adaptation_set(
            as_id,
            sub,
            params.proxy_base,
        ));
        as_id += 1;
    }

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
     xsi:schemaLocation="urn:mpeg:dash:schema:mpd:2011 DASH-MPD.xsd"
     profiles="urn:mpeg:dash:profile:isoff-live:2011"
     {}
     minBufferTime="PT2S">
  <Period id="1" start="PT0S">
{}  </Period>
</MPD>"#,
        timing_attrs, adaptation_sets
    )
}

fn generate_video_adaptation_set(
    id: usize,
    reps: &[RepresentationData<'_>],
    proxy_base: &str,
    transmux_ts: bool,
) -> String {
    // Check for DRM (AES-128) in any representation.
    let content_protection = reps.iter().find_map(|r| {
        crate::hls::first_key_uri(r.media_playlist).map(|key_uri| {
            let proxied = proxy_url(&key_uri, proxy_base);
            format!(
                r#"    <ContentProtection schemeIdUri="urn:ietf:rfc:8216" value="AES-128">
      <ms:pro>{}</ms:pro>
    </ContentProtection>
"#,
                xml_escape(&proxied)
            )
        })
    });

    let mut representations = String::new();
    for rep in reps {
        representations.push_str(&generate_video_representation(rep, proxy_base, transmux_ts));
    }

    format!(
        r#"    <AdaptationSet id="{}" contentType="video" segmentAlignment="true" bitstreamSwitching="true">
{}{}    </AdaptationSet>
"#,
        id,
        content_protection.unwrap_or_default(),
        representations
    )
}

fn generate_video_representation(rep: &RepresentationData<'_>, proxy_base: &str, transmux_ts: bool) -> String {
    let mime_type = if rep.is_fmp4 {
        "video/mp4"
    } else if transmux_ts {
        "video/mp4"
    } else {
        "video/MP2T"
    };
    let bandwidth = rep.variant.bandwidth;

    let codecs_attr = rep
        .variant
        .codecs
        .as_deref()
        .map(|c| format!(r#" codecs="{}""#, xml_escape(c)))
        .unwrap_or_default();

    let resolution_attrs = rep
        .variant
        .resolution
        .as_ref()
        .map(|r| format!(r#" width="{}" height="{}""#, r.width, r.height))
        .unwrap_or_default();

    let target_dur_ms = (rep.media_playlist.target_duration as u64) * 1000;

    let first_seg_url = if !rep.is_fmp4 && transmux_ts {
        if rep.media_playlist.end_list {
            // VOD: first segment is stable, reference it directly.
            rep.media_playlist.segments.first().and_then(|seg| {
                resolve_segment_url(&rep.playlist_url, &seg.uri)
                    .ok()
                    .map(|u| proxy_init_url(u.as_str(), proxy_base))
            })
        } else {
            // Live: segments expire quickly; point at the playlist so the init
            // handler always fetches the latest available segment.
            Some(proxy_init_from_playlist_url(rep.playlist_url.as_str(), proxy_base))
        }
    } else {
        None
    };

    let segment_list = generate_segment_list(
        rep.media_playlist,
        &rep.playlist_url,
        rep.is_fmp4,
        target_dur_ms,
        proxy_base,
        first_seg_url,
        transmux_ts,
    );

    format!(
        r#"      <Representation id="{}" mimeType="{}"{}{}  bandwidth="{}">
{}      </Representation>
"#,
        xml_escape(&rep.id),
        mime_type,
        codecs_attr,
        resolution_attrs,
        bandwidth,
        segment_list
    )
}

fn generate_audio_adaptation_set(id: usize, rep: &AltRepData<'_>, proxy_base: &str, transmux_ts: bool) -> String {
    let lang = rep.alt.language.as_deref().unwrap_or("und");
    let label = &rep.alt.name;

    let representation = if let (Some(pl), Some(url)) = (rep.media_playlist, &rep.playlist_url) {
        let mime_type = if rep.is_fmp4 {
            "audio/mp4"
        } else if transmux_ts {
            "audio/mp4"
        } else {
            "audio/MP2T"
        };
        let target_dur_ms = (pl.target_duration as u64) * 1000;

        let first_seg_url = if !rep.is_fmp4 && transmux_ts {
            if pl.end_list {
                pl.segments.first().and_then(|seg| {
                    resolve_segment_url(url, &seg.uri)
                        .ok()
                        .map(|u| proxy_init_url(u.as_str(), proxy_base))
                })
            } else {
                Some(proxy_init_from_playlist_url(url.as_str(), proxy_base))
            }
        } else {
            None
        };

        let segment_list = generate_segment_list(pl, url, rep.is_fmp4, target_dur_ms, proxy_base, first_seg_url, transmux_ts);
        format!(
            r#"      <Representation id="{}" mimeType="{}" bandwidth="128000">
{}      </Representation>
"#,
            xml_escape(&rep.id),
            mime_type,
            segment_list
        )
    } else {
        String::new()
    };

    format!(
        r#"    <AdaptationSet id="{}" contentType="audio" lang="{}" label="{}">
{}    </AdaptationSet>
"#,
        id,
        xml_escape(lang),
        xml_escape(label),
        representation
    )
}

fn generate_subtitle_adaptation_set(
    id: usize,
    rep: &AltRepData<'_>,
    proxy_base: &str,
) -> String {
    let lang = rep.alt.language.as_deref().unwrap_or("und");
    let label = &rep.alt.name;

    let representation = if let (Some(pl), Some(url)) = (rep.media_playlist, &rep.playlist_url) {
        let target_dur_ms = (pl.target_duration as u64) * 1000;
        let segment_list = generate_segment_list(pl, url, false, target_dur_ms, proxy_base, None, false);
        format!(
            r#"      <Representation id="{}" mimeType="text/vtt" bandwidth="10000">
{}      </Representation>
"#,
            xml_escape(&rep.id),
            segment_list
        )
    } else {
        String::new()
    };

    format!(
        r#"    <AdaptationSet id="{}" contentType="text" lang="{}" label="{}" mimeType="text/vtt">
{}    </AdaptationSet>
"#,
        id,
        xml_escape(lang),
        xml_escape(label),
        representation
    )
}

fn generate_segment_list(
    pl: &MediaPlaylist,
    base_url: &Url,
    is_fmp4: bool,
    target_dur_ms: u64,
    proxy_base: &str,
    first_seg_url: Option<String>,
    transmux_ts: bool,
) -> String {
    let mut lines = format!(
        r#"        <SegmentList timescale="1000" duration="{}">
"#,
        target_dur_ms
    );

    // Initialization segment: fMP4 uses the map entry; transmuxed TS uses the pre-computed init URL.
    if is_fmp4 {
        if let Some(map_uri) = pl
            .segments
            .iter()
            .find_map(|s| s.map.as_ref().map(|m| m.uri.as_str()))
        {
            if let Ok(abs_url) = resolve_segment_url(base_url, map_uri) {
                let proxied = proxy_url(abs_url.as_str(), proxy_base);
                lines.push_str(&format!(
                    r#"          <Initialization sourceURL="{}"/>
"#,
                    xml_escape(&proxied)
                ));
            }
        }
    } else if let Some(init_url) = first_seg_url {
        lines.push_str(&format!(
            r#"          <Initialization sourceURL="{}"/>
"#,
            xml_escape(&init_url)
        ));
    }

    // Segment URLs.
    for (idx, seg) in pl.segments.iter().enumerate() {
        let proxied = if !is_fmp4 && transmux_ts {
            // For TS+transmux, route through the playlist-aware endpoint so FFmpeg
            // can handle AES-128 decryption via the HLS demuxer.
            proxy_ts_pl_url(base_url.as_str(), idx, target_dur_ms, proxy_base)
        } else if let Ok(abs_url) = resolve_segment_url(base_url, &seg.uri) {
            proxy_url(abs_url.as_str(), proxy_base)
        } else {
            continue;
        };
        let dur_ms = (seg.duration * 1000.0) as u64;
        lines.push_str(&format!(
            r#"          <SegmentURL media="{}" duration="{}"/>
"#,
            xml_escape(&proxied),
            dur_ms
        ));
    }

    lines.push_str("        </SegmentList>\n");
    lines
}
