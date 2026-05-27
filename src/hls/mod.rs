use crate::error::AppError;
use bytes::Bytes;
use m3u8_rs::{parse_playlist_res, AlternativeMedia, MasterPlaylist, MediaPlaylist, Playlist};

/// Result of parsing an HLS playlist byte buffer.
pub enum ParsedPlaylist {
    Master(MasterPlaylist),
    Media(MediaPlaylist),
}

/// Parse raw HLS bytes into a `ParsedPlaylist`.
pub fn parse_playlist(data: &Bytes) -> Result<ParsedPlaylist, AppError> {
    match parse_playlist_res(data) {
        Ok(Playlist::MasterPlaylist(pl)) => Ok(ParsedPlaylist::Master(pl)),
        Ok(Playlist::MediaPlaylist(pl)) => Ok(ParsedPlaylist::Media(pl)),
        Err(e) => Err(AppError::ParseError(format!(
            "m3u8 parse failure: {:?}",
            e
        ))),
    }
}

/// Determine if a media playlist uses fMP4 segments (any segment has a `map` field).
pub fn is_fmp4(pl: &MediaPlaylist) -> bool {
    pl.segments.iter().any(|seg| seg.map.is_some())
}

/// Compute total duration of all segments in seconds (VOD).
pub fn total_duration(pl: &MediaPlaylist) -> f64 {
    pl.segments.iter().map(|s| s.duration as f64).sum()
}

/// Return the first AES-128 key URI found in a media playlist, if any.
pub fn first_key_uri(pl: &MediaPlaylist) -> Option<String> {
    for seg in &pl.segments {
        if let Some(key) = &seg.key {
            use m3u8_rs::KeyMethod;
            if matches!(key.method, KeyMethod::AES128) {
                if let Some(uri) = &key.uri {
                    return Some(uri.clone());
                }
            }
        }
    }
    None
}

/// Collect audio alternative media entries from a master playlist.
pub fn audio_alternatives(master: &MasterPlaylist) -> Vec<&AlternativeMedia> {
    master
        .alternatives
        .iter()
        .filter(|a| matches!(a.media_type, m3u8_rs::AlternativeMediaType::Audio))
        .collect()
}

/// Collect subtitle alternative media entries from a master playlist.
pub fn subtitle_alternatives(master: &MasterPlaylist) -> Vec<&AlternativeMedia> {
    master
        .alternatives
        .iter()
        .filter(|a| matches!(a.media_type, m3u8_rs::AlternativeMediaType::Subtitles))
        .collect()
}
