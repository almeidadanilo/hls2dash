use anyhow::anyhow;
use bytes::Bytes;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Transmux MPEG-TS bytes to fragmented MP4 via ffmpeg.
/// Returns the full fMP4 output (ftyp + moov + moof + mdat).
pub async fn transmux_ts(ts_bytes: Bytes) -> anyhow::Result<Bytes> {
    transmux_ts_with_offset(ts_bytes, 0.0).await
}

/// Like `transmux_ts` but shifts all output timestamps by `offset_secs` seconds.
/// Use this for media segments so each segment's tfdt reflects its position in the
/// presentation timeline rather than the per-segment PTS reset (typically 1 s).
pub async fn transmux_ts_with_offset(ts_bytes: Bytes, offset_secs: f64) -> anyhow::Result<Bytes> {
    let offset_str = format!("{:.6}", offset_secs);
    let mut child = Command::new("ffmpeg")
        .args(["-loglevel", "error", "-i", "pipe:0", "-c", "copy", "-bsf:a", "aac_adtstoasc"])
        .args(["-output_ts_offset", offset_str.as_str()])
        .args(["-movflags", "frag_keyframe+default_base_moof", "-f", "mp4", "pipe:1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("failed to spawn ffmpeg: {}. Is ffmpeg installed and in PATH?", e))?;

    let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("ffmpeg stdin unavailable"))?;
    stdin.write_all(&ts_bytes).await?;
    drop(stdin);

    let output = child.wait_with_output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("ffmpeg transmux failed: {}", stderr));
    }
    Ok(Bytes::from(output.stdout))
}

/// Transmux an HLS media playlist URL to fragmented MP4 via ffmpeg's HLS demuxer.
#[allow(dead_code)]
/// FFmpeg fetches the segments itself and handles AES-128 decryption automatically.
/// `-t 15` ensures we stop after one or two segments rather than streaming indefinitely.
pub async fn transmux_ts_from_url(url: &str) -> anyhow::Result<Bytes> {
    let child = Command::new("ffmpeg")
        .args([
            "-loglevel", "error",
            "-allowed_extensions", "ALL",
            "-protocol_whitelist", "file,http,https,tcp,tls,crypto",
            "-i", url,
            "-t", "15",
            "-c", "copy",
            "-movflags", "frag_keyframe+default_base_moof",
            "-f", "mp4",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("failed to spawn ffmpeg: {}. Is ffmpeg installed and in PATH?", e))?;

    let output = child.wait_with_output().await?;
    if output.stdout.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("ffmpeg produced no output from URL: {}", stderr));
    }
    Ok(Bytes::from(output.stdout))
}

/// Transmux a single TS segment URL directly to fMP4.
/// FFmpeg fetches the segment itself via HTTPS — no stdin pipe, no temp file.
/// Works for unencrypted streams. For AES-128, use transmux_ts_from_file with a mini M3U8.
#[allow(dead_code)]
pub async fn transmux_ts_from_segment_url(url: &str) -> anyhow::Result<Bytes> {
    let child = Command::new("ffmpeg")
        .args([
            "-loglevel", "error",
            // No -allowed_extensions here: that flag is HLS-demuxer-only and
            // causes "Option not found" when the input is a raw .ts HTTPS URL.
            "-protocol_whitelist", "file,http,https,tcp,tls,crypto",
            "-i", url,
            "-c", "copy",
            // ADTS→ASC: MPEG-TS carries AAC in ADTS format; MP4 requires ASC.
            "-bsf:a", "aac_adtstoasc",
            // empty_moov forces fMP4 container mode immediately, preventing fallback
            // to non-fragmented MP4 when the MPEG-TS demuxer misses keyframe flags.
            "-movflags", "empty_moov+frag_keyframe+default_base_moof",
            "-f", "mp4",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("failed to spawn ffmpeg: {}", e))?;

    let output = child.wait_with_output().await?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        tracing::warn!(ffmpeg_stderr = %stderr, "ffmpeg segment stderr");
    }
    let bytes = Bytes::from(output.stdout);
    let has_moof_toplevel = extract_media(&bytes).is_some();
    let has_moof_anywhere = bytes.windows(4).any(|w| w == b"moof");
    // Log first 64 bytes as hex to inspect the box structure
    let prefix: Vec<String> = bytes.iter().take(64).map(|b| format!("{:02x}", b)).collect();
    tracing::debug!(
        output_bytes = bytes.len(),
        has_moof_toplevel,
        has_moof_anywhere,
        first_64_bytes = %prefix.join(" "),
        "ffmpeg segment URL output"
    );
    if bytes.is_empty() {
        return Err(anyhow!("ffmpeg produced no output for segment URL: {}", stderr));
    }
    Ok(bytes)
}

/// Transmux a single-segment mini-playlist file to fMP4.
/// Used for AES-128 encrypted segments where the key context is embedded in the M3U8.
#[allow(dead_code)]
pub async fn transmux_ts_from_file(path: &str) -> anyhow::Result<Bytes> {
    transmux_ts_from_file_with_offset(path, 0.0).await
}

/// Like `transmux_ts_from_file` but shifts all output timestamps by `offset_secs` seconds.
pub async fn transmux_ts_from_file_with_offset(path: &str, offset_secs: f64) -> anyhow::Result<Bytes> {
    let offset_str = format!("{:.6}", offset_secs);
    let child = Command::new("ffmpeg")
        .args([
            "-loglevel", "error",
            "-allowed_extensions", "ALL",
            "-protocol_whitelist", "file,http,https,tcp,tls,crypto",
            "-i", path,
            "-c", "copy",
            "-bsf:a", "aac_adtstoasc",
            "-output_ts_offset", offset_str.as_str(),
            "-movflags", "empty_moov+frag_keyframe+default_base_moof",
            "-f", "mp4",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("failed to spawn ffmpeg: {}", e))?;

    let output = child.wait_with_output().await?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        tracing::warn!(ffmpeg_stderr = %stderr, "ffmpeg mini-playlist stderr");
    }
    if output.stdout.is_empty() {
        return Err(anyhow!("ffmpeg produced no output from mini-playlist: {}", stderr));
    }
    Ok(Bytes::from(output.stdout))
}

/// Extract init segment (ftyp + moov) — everything before the first `moof` box.
pub fn extract_init(fmp4: &[u8]) -> Option<Bytes> {
    find_box_offset(fmp4, b"moof").map(|pos| Bytes::copy_from_slice(&fmp4[..pos]))
}

/// Zero out duration fields in mvhd, tkhd, and mdhd boxes within an init segment.
///
/// Without this, Chrome MSE treats the moov duration (which FFmpeg sets to the first
/// segment's length) as appendWindowEnd. Any media data timestamped beyond that value
/// is silently discarded, causing playback to stall after the first segment.
pub fn patch_moov_duration(init: &[u8]) -> Bytes {
    let mut data = init.to_vec();
    let len = data.len();
    patch_boxes(&mut data, 0, len);
    Bytes::from(data)
}

fn patch_boxes(data: &mut [u8], start: usize, end: usize) {
    let mut pos = start;
    while pos + 8 <= end {
        let size = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
            as usize;
        if size < 8 || pos + size > end {
            break;
        }
        let tag = [data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]];
        match &tag {
            b"moov" | b"trak" | b"mdia" => {
                patch_boxes(data, pos + 8, pos + size);
            }
            b"mvhd" | b"mdhd" => {
                // version=0: duration at +24 (4 bytes); version=1: at +32 (8 bytes)
                if pos + 9 <= end {
                    let (off, len) = if data[pos + 8] == 0 { (pos + 24, 4) } else { (pos + 32, 8) };
                    if off + len <= end {
                        data[off..off + len].fill(0);
                    }
                }
            }
            b"tkhd" => {
                // version=0: duration at +28 (4 bytes); version=1: at +36 (8 bytes)
                if pos + 9 <= end {
                    let (off, len) = if data[pos + 8] == 0 { (pos + 28, 4) } else { (pos + 36, 8) };
                    if off + len <= end {
                        data[off..off + len].fill(0);
                    }
                }
            }
            _ => {}
        }
        pos += size;
    }
}

/// Rewrite the baseMediaDecodeTime in every tfdt box found inside every traf in the data,
/// and extend the last sample's duration in each trun so the segment fills exactly the
/// declared EXTINF window.
///
/// Source HLS segments all start at PTS = 1 second in their track timescale (e.g. 90000
/// for H.264 at 90 kHz, 44100/48000 for AAC). Because every independent TS transmux
/// produces the same tfdt, the DASH player overwrites the same buffer position for every
/// segment. This function sets each tfdt to `cumulative_ms × timescale / 1000` where
/// cumulative_ms is the sum of actual #EXTINF durations for all preceding segments.
///
/// Some source HLS segments are sparse: EXTINF declares e.g. 4 s but the actual media
/// frames only span ~1 s. The remaining 3 s appear as an MSE buffer hole, causing the
/// gap controller to seek repeatedly. Extending the last sample's decode duration to fill
/// the full EXTINF window eliminates those holes without changing any box sizes.
pub fn patch_media_timestamps(media: &[u8], cumulative_ms: u64, extinf_ms: u64) -> Bytes {
    let mut data = media.to_vec();
    let len = data.len();
    let mut pos = 0;
    while pos + 8 <= len {
        let size = u32::from_be_bytes(
            data[pos..pos + 4].try_into().unwrap_or([0; 4])
        ) as usize;
        if size < 8 || pos + size > len {
            break;
        }
        if &data[pos + 4..pos + 8] == b"moof" {
            patch_moof_tfdts(&mut data, pos, pos + size, cumulative_ms, extinf_ms);
        }
        pos += size;
    }
    Bytes::from(data)
}

fn patch_moof_tfdts(data: &mut [u8], moof_start: usize, moof_end: usize, cumulative_ms: u64, extinf_ms: u64) {
    let mut pos = moof_start + 8;
    while pos + 8 <= moof_end {
        let size = u32::from_be_bytes(
            data[pos..pos + 4].try_into().unwrap_or([0; 4])
        ) as usize;
        if size < 8 || pos + size > moof_end {
            break;
        }
        if &data[pos + 4..pos + 8] == b"traf" {
            patch_traf_tfdt(data, pos, pos + size, cumulative_ms, extinf_ms);
        }
        pos += size;
    }
}

fn patch_traf_tfdt(data: &mut [u8], traf_start: usize, traf_end: usize, cumulative_ms: u64, extinf_ms: u64) {
    let mut timescale: u64 = 0;
    let mut pos = traf_start + 8;
    while pos + 8 <= traf_end {
        let size = u32::from_be_bytes(
            data[pos..pos + 4].try_into().unwrap_or([0; 4])
        ) as usize;
        if size < 8 || pos + size > traf_end {
            break;
        }
        if &data[pos + 4..pos + 8] == b"tfdt" {
            let version = data[pos + 8];
            if version == 0 && pos + 16 <= traf_end {
                // cur == track_timescale (source PTS == 1 s == timescale ticks)
                let cur = u32::from_be_bytes(data[pos + 12..pos + 16].try_into().unwrap_or([0; 4])) as u64;
                timescale = cur;
                let new_val = ((cumulative_ms * cur / 1000) as u32).to_be_bytes();
                data[pos + 12..pos + 16].copy_from_slice(&new_val);
            } else if version == 1 && pos + 20 <= traf_end {
                let cur = u64::from_be_bytes(data[pos + 12..pos + 20].try_into().unwrap_or([0; 8]));
                timescale = cur;
                let new_val = (cumulative_ms * cur / 1000).to_be_bytes();
                data[pos + 12..pos + 20].copy_from_slice(&new_val);
            }
            break; // only one tfdt per traf
        }
        pos += size;
    }
    if timescale > 0 && extinf_ms > 0 {
        let expected_ticks = extinf_ms * timescale / 1000;
        extend_traf_last_sample(data, traf_start, traf_end, expected_ticks);
    }
}

/// If the sum of sample durations in the trun box is less than `expected_ticks`, extend the
/// last sample's duration to cover the full expected window. This prevents MSE buffer holes
/// in source segments whose actual media frames span less than the declared EXTINF duration.
///
/// Only operates when trun carries per-sample duration (flags bit 0x100 set). Skips silently
/// if the format differs (e.g. default_sample_duration in tfhd), leaving the segment untouched.
fn extend_traf_last_sample(data: &mut [u8], traf_start: usize, traf_end: usize, expected_ticks: u64) {
    let mut pos = traf_start + 8;
    while pos + 8 <= traf_end {
        let size = u32::from_be_bytes(
            data[pos..pos + 4].try_into().unwrap_or([0; 4])
        ) as usize;
        if size < 8 || pos + size > traf_end {
            break;
        }
        if &data[pos + 4..pos + 8] == b"trun" {
            let flags = u32::from_be_bytes([0, data[pos + 9], data[pos + 10], data[pos + 11]]);
            let sample_count = u32::from_be_bytes(
                data[pos + 12..pos + 16].try_into().unwrap_or([0; 4])
            ) as usize;

            let has_data_offset        = (flags & 0x001) != 0;
            let has_first_sample_flags = (flags & 0x004) != 0;
            let has_sample_duration    = (flags & 0x100) != 0;
            let has_sample_size        = (flags & 0x200) != 0;
            let has_sample_flags       = (flags & 0x400) != 0;
            let has_cto                = (flags & 0x800) != 0;

            if !has_sample_duration || sample_count == 0 {
                break;
            }

            let stride = 4 * (has_sample_duration as usize
                + has_sample_size as usize
                + has_sample_flags as usize
                + has_cto as usize);
            if stride == 0 {
                break;
            }

            let entry_base = pos + 16
                + if has_data_offset { 4 } else { 0 }
                + if has_first_sample_flags { 4 } else { 0 };

            let mut total: u64 = 0;
            let mut off = entry_base;
            for _ in 0..sample_count {
                if off + 4 > traf_end {
                    break;
                }
                total += u32::from_be_bytes(
                    data[off..off + 4].try_into().unwrap_or([0; 4])
                ) as u64;
                off += stride;
            }

            if total < expected_ticks {
                let last_dur_off = entry_base + (sample_count - 1) * stride;
                if last_dur_off + 4 <= traf_end {
                    let old_dur = u32::from_be_bytes(
                        data[last_dur_off..last_dur_off + 4].try_into().unwrap_or([0; 4])
                    ) as u64;
                    let new_dur = ((old_dur + expected_ticks - total) as u32).to_be_bytes();
                    data[last_dur_off..last_dur_off + 4].copy_from_slice(&new_dur);
                }
            }
            break; // one trun per traf is standard FFmpeg output
        }
        pos += size;
    }
}

/// Extract media segment — everything from the first `moof` box onwards.
pub fn extract_media(fmp4: &[u8]) -> Option<Bytes> {
    find_box_offset(fmp4, b"moof").map(|pos| Bytes::copy_from_slice(&fmp4[pos..]))
}

/// Read the baseMediaDecodeTime from the first tfdt box in the first traf in the first moof.
/// Returns None if the structure is not found or malformed.
pub fn read_tfdt(media: &[u8]) -> Option<u64> {
    let moof_off = find_box_offset(media, b"moof")?;
    let moof_size = u32::from_be_bytes(media.get(moof_off..moof_off + 4)?.try_into().ok()?) as usize;
    let moof_end = moof_off + moof_size;
    let traf_off = find_box_in_range(media, moof_off + 8, moof_end, b"traf")?;
    let traf_size = u32::from_be_bytes(media.get(traf_off..traf_off + 4)?.try_into().ok()?) as usize;
    let traf_end = traf_off + traf_size;
    let tfdt_off = find_box_in_range(media, traf_off + 8, traf_end, b"tfdt")?;
    let version = *media.get(tfdt_off + 8)?;
    if version == 0 {
        let t = media.get(tfdt_off + 12..tfdt_off + 16)?;
        Some(u32::from_be_bytes(t.try_into().ok()?) as u64)
    } else {
        let t = media.get(tfdt_off + 12..tfdt_off + 20)?;
        Some(u64::from_be_bytes(t.try_into().ok()?))
    }
}

/// Return the byte offset of the first MP4 box with the given 4-byte type tag (top-level scan).
fn find_box_offset(data: &[u8], box_type: &[u8; 4]) -> Option<usize> {
    let mut pos = 0;
    while pos + 8 <= data.len() {
        let size = u32::from_be_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
        if &data[pos + 4..pos + 8] == box_type {
            return Some(pos);
        }
        if size < 8 || pos + size > data.len() {
            break;
        }
        pos += size;
    }
    None
}

/// Scan for a box of the given type within [start, end) of data.
fn find_box_in_range(data: &[u8], start: usize, end: usize, box_type: &[u8; 4]) -> Option<usize> {
    let mut pos = start;
    let limit = end.min(data.len());
    while pos + 8 <= limit {
        let size = u32::from_be_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
        if &data[pos + 4..pos + 8] == box_type {
            return Some(pos);
        }
        if size < 8 || pos + size > limit {
            break;
        }
        pos += size;
    }
    None
}
