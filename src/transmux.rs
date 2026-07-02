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
        // empty_moov is required: without it FFmpeg writes the first GOP as a
        // conventional (non-fragmented) sample table with its data in a plain mdat
        // that sits *before* the first moof box. extract_media() below only keeps
        // data from the first moof onward, so without empty_moov the entire first
        // GOP of every segment is silently dropped — the client only ever receives
        // the later GOPs, cutting each segment short by however long its first GOP
        // lasted and forcing an MSE gap-jump at every segment boundary.
        //
        // delay_moov is also required together with empty_moov: without it, FFmpeg
        // writes the moov before the aac_adtstoasc bitstream filter has populated the
        // AAC AudioSpecificConfig, producing an audio `esds` box with a truncated
        // DecoderConfigDescriptor (missing DecoderSpecificInfo entirely). Chrome's MSE
        // demuxer rejects that outright with CHUNK_DEMUXER_ERROR_APPEND_FAILED /
        // "stream parsing failed" on the very first appendBuffer call. delay_moov
        // defers writing moov until the first fragment is ready, by which point the
        // bsf has seen an ADTS frame and set the real extradata.
        .args(["-movflags", "empty_moov+delay_moov+frag_keyframe+default_base_moof", "-f", "mp4", "pipe:1"])
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
            // delay_moov must accompany it: without it, the moov is written before
            // aac_adtstoasc has populated the AAC extradata, producing an esds box
            // Chrome's MSE demuxer rejects outright (see transmux_ts_with_offset).
            "-movflags", "empty_moov+delay_moov+frag_keyframe+default_base_moof",
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
            // delay_moov: see transmux_ts_with_offset — required alongside empty_moov
            // whenever aac_adtstoasc is in play, or the audio esds ends up missing its
            // DecoderSpecificInfo and Chrome's MSE demuxer rejects the whole segment.
            "-movflags", "empty_moov+delay_moov+frag_keyframe+default_base_moof",
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

/// Read each track's real timescale from its `mdhd` box in the `moov`, keyed by the
/// track_ID from the corresponding `tkhd`. This is the authoritative source for timescale
/// — do not infer it from tfdt values (see `patch_media_timestamps`).
pub fn read_track_timescales(fmp4: &[u8]) -> std::collections::HashMap<u32, u64> {
    use std::collections::HashMap;
    let mut out = HashMap::new();
    let Some(moov_off) = find_box_offset(fmp4, b"moov") else {
        return out;
    };
    let moov_size = match fmp4.get(moov_off..moov_off + 4).and_then(|s| s.try_into().ok()) {
        Some(b) => u32::from_be_bytes(b) as usize,
        None => return out,
    };
    let moov_end = moov_off + moov_size;
    let mut pos = moov_off + 8;
    while pos + 8 <= moov_end {
        let size = u32::from_be_bytes(fmp4[pos..pos + 4].try_into().unwrap_or([0; 4])) as usize;
        if size < 8 || pos + size > moov_end {
            break;
        }
        if &fmp4[pos + 4..pos + 8] == b"trak" {
            let trak_end = pos + size;
            if let (Some(track_id), Some(timescale)) = (
                find_box_in_range(fmp4, pos + 8, trak_end, b"tkhd").and_then(|off| {
                    // tkhd fullbox: version(1)+flags(3), then creation_time/modification_time
                    // (4 bytes each for v0, 8 bytes each for v1) precede track_ID — unlike
                    // tfhd, where track_ID sits immediately after the flags.
                    let version = *fmp4.get(off + 8)?;
                    let id_off = if version == 0 { off + 20 } else { off + 28 };
                    fmp4.get(id_off..id_off + 4)
                })
                    .and_then(|b| b.try_into().ok())
                    .map(u32::from_be_bytes),
                find_box_in_range(fmp4, pos + 8, trak_end, b"mdia").and_then(|mdia_off| {
                    let mdia_size = u32::from_be_bytes(
                        fmp4[mdia_off..mdia_off + 4].try_into().unwrap_or([0; 4]),
                    ) as usize;
                    find_box_in_range(fmp4, mdia_off + 8, mdia_off + mdia_size, b"mdhd")
                        .and_then(|off| fmp4.get(off + 8).map(|&v| (off, v)))
                        .and_then(|(off, version)| {
                            let ts_off = if version == 0 { off + 20 } else { off + 28 };
                            fmp4.get(ts_off..ts_off + 4)
                        })
                        .and_then(|b| b.try_into().ok())
                        .map(u32::from_be_bytes)
                }),
            ) {
                out.insert(track_id, timescale as u64);
            }
        }
        pos += size;
    }
    out
}

/// Rewrite the baseMediaDecodeTime in every tfdt box found inside every traf in the data,
/// and extend the last sample's duration in the final fragment so the segment fills
/// exactly the declared EXTINF window.
///
/// Every independent TS-segment transmux starts its own decode timeline (FFmpeg picks
/// whatever start offset it likes — sometimes 0, sometimes a 1 s reset — depending on
/// muxer flags), so left unpatched, the DASH player would overwrite the same buffer
/// position for every segment. This function shifts every tfdt in the segment by a
/// constant per-track delta so that the *first* fragment lands at
/// `cumulative_ms × timescale / 1000` — the sum of actual #EXTINF durations for all
/// preceding segments — while preserving the original spacing between fragments. The
/// timescale must come from the real `mdhd` box (see `read_track_timescales`): the raw
/// tfdt value cannot be assumed to equal the timescale, since that only held for one
/// specific FFmpeg configuration and silently produced near-zero (wrong) deltas once the
/// transmux flags changed.
///
/// A single TS segment can produce more than one moof/traf pair: FFmpeg's
/// `frag_keyframe` starts a new fragment at every keyframe, so a segment whose GOP is
/// shorter than its EXTINF duration (e.g. 2 s GOP inside a 6 s segment) yields several
/// fragments. We compute the delta once per track from the first fragment only, then add
/// (not overwrite) it everywhere, preserving FFmpeg's correct internal spacing between
/// fragments.
///
/// Some source HLS segments are sparse: EXTINF declares e.g. 4 s but the actual media
/// frames only span ~1 s. The remaining time appears as an MSE buffer hole, causing the
/// gap controller to seek repeatedly. Extending the last sample's decode duration in the
/// final fragment to fill the full EXTINF window eliminates those holes without changing
/// any box sizes.
pub fn patch_media_timestamps(
    media: &[u8],
    timescales: &std::collections::HashMap<u32, u64>,
    cumulative_ms: u64,
    extinf_ms: u64,
) -> Bytes {
    use std::collections::HashMap;

    let mut data = media.to_vec();
    let len = data.len();

    let mut moofs = Vec::new();
    let mut pos = 0;
    while pos + 8 <= len {
        let size = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap_or([0; 4])) as usize;
        if size < 8 || pos + size > len {
            break;
        }
        if &data[pos + 4..pos + 8] == b"moof" {
            moofs.push((pos, pos + size));
        }
        pos += size;
    }
    if moofs.is_empty() {
        return Bytes::from(data);
    }

    // Per-track additive delta, fixed from the first fragment seen for that track so
    // every later fragment's original spacing relative to it is preserved.
    let mut delta_of: HashMap<u32, i128> = HashMap::new();
    // Byte range of the most recently seen traf per track, so we know which fragment is
    // "last" for the EXTINF hole-filling extension.
    let mut last_traf_of: HashMap<u32, (usize, usize)> = HashMap::new();
    // Running total of decoded ticks per track, across every fragment, for the hole check.
    let mut total_ticks_of: HashMap<u32, u64> = HashMap::new();

    for &(moof_start, moof_end) in &moofs {
        let mut p = moof_start + 8;
        while p + 8 <= moof_end {
            let size = u32::from_be_bytes(data[p..p + 4].try_into().unwrap_or([0; 4])) as usize;
            if size < 8 || p + size > moof_end {
                break;
            }
            if &data[p + 4..p + 8] == b"traf" {
                let traf_start = p;
                let traf_end = p + size;
                if let Some(track_id) = read_tfhd_track_id(&data, traf_start, traf_end) {
                    last_traf_of.insert(track_id, (traf_start, traf_end));
                    *total_ticks_of.entry(track_id).or_insert(0) +=
                        sum_trun_durations(&data, traf_start, traf_end);

                    if let Some(tfdt_off) = find_box_in_range(&data, traf_start + 8, traf_end, b"tfdt") {
                        let version = data[tfdt_off + 8];
                        if version == 0 && tfdt_off + 16 <= traf_end {
                            let old = u32::from_be_bytes(
                                data[tfdt_off + 12..tfdt_off + 16].try_into().unwrap_or([0; 4]),
                            ) as i128;
                            let timescale = *timescales.get(&track_id).unwrap_or(&90_000) as i128;
                            let delta = *delta_of
                                .entry(track_id)
                                .or_insert_with(|| cumulative_ms as i128 * timescale / 1000 - old);
                            let new_val = (old + delta).max(0) as u32;
                            data[tfdt_off + 12..tfdt_off + 16].copy_from_slice(&new_val.to_be_bytes());
                        } else if version == 1 && tfdt_off + 20 <= traf_end {
                            let old = u64::from_be_bytes(
                                data[tfdt_off + 12..tfdt_off + 20].try_into().unwrap_or([0; 8]),
                            ) as i128;
                            let timescale = *timescales.get(&track_id).unwrap_or(&90_000) as i128;
                            let delta = *delta_of
                                .entry(track_id)
                                .or_insert_with(|| cumulative_ms as i128 * timescale / 1000 - old);
                            let new_val = (old + delta).max(0) as u64;
                            data[tfdt_off + 12..tfdt_off + 20].copy_from_slice(&new_val.to_be_bytes());
                        }
                    }
                }
            }
            p += size;
        }
    }

    if extinf_ms > 0 {
        for (track_id, (traf_start, traf_end)) in last_traf_of {
            if let Some(&timescale) = timescales.get(&track_id) {
                let expected_ticks = extinf_ms * timescale / 1000;
                let total = total_ticks_of.get(&track_id).copied().unwrap_or(0);
                if total < expected_ticks {
                    extend_traf_last_sample(&mut data, traf_start, traf_end, expected_ticks - total);
                }
            }
        }
    }

    Bytes::from(data)
}

/// Read the track_ID field from the tfhd box inside a traf, used to correlate fragments
/// belonging to the same track across multiple moof boxes in one segment.
fn read_tfhd_track_id(data: &[u8], traf_start: usize, traf_end: usize) -> Option<u32> {
    let tfhd_off = find_box_in_range(data, traf_start + 8, traf_end, b"tfhd")?;
    let bytes = data.get(tfhd_off + 12..tfhd_off + 16)?;
    Some(u32::from_be_bytes(bytes.try_into().ok()?))
}

/// Sum the per-sample durations declared in a traf's trun box, without modifying anything.
/// Returns 0 if the trun uses default_sample_duration (from tfhd) instead of per-sample
/// durations, matching the same limitation as `extend_traf_last_sample`.
fn sum_trun_durations(data: &[u8], traf_start: usize, traf_end: usize) -> u64 {
    let Some(trun_off) = find_box_in_range(data, traf_start + 8, traf_end, b"trun") else {
        return 0;
    };
    let flags = u32::from_be_bytes([0, data[trun_off + 9], data[trun_off + 10], data[trun_off + 11]]);
    let sample_count = u32::from_be_bytes(
        data.get(trun_off + 12..trun_off + 16)
            .and_then(|s| s.try_into().ok())
            .unwrap_or([0; 4]),
    ) as usize;

    let has_data_offset = (flags & 0x001) != 0;
    let has_first_sample_flags = (flags & 0x004) != 0;
    let has_sample_duration = (flags & 0x100) != 0;
    let has_sample_size = (flags & 0x200) != 0;
    let has_sample_flags = (flags & 0x400) != 0;
    let has_cto = (flags & 0x800) != 0;

    if !has_sample_duration || sample_count == 0 {
        return 0;
    }

    let stride = 4 * (has_sample_duration as usize
        + has_sample_size as usize
        + has_sample_flags as usize
        + has_cto as usize);
    if stride == 0 {
        return 0;
    }

    let entry_base = trun_off + 16
        + if has_data_offset { 4 } else { 0 }
        + if has_first_sample_flags { 4 } else { 0 };

    let mut total: u64 = 0;
    let mut off = entry_base;
    for _ in 0..sample_count {
        if off + 4 > traf_end {
            break;
        }
        total += u32::from_be_bytes(data[off..off + 4].try_into().unwrap_or([0; 4])) as u64;
        off += stride;
    }
    total
}

/// Extend the last sample's duration in this traf's trun box by `shortfall_ticks`. Used to
/// close MSE buffer holes when a segment's actual media (possibly spread across several
/// fragments) spans less than the declared EXTINF duration — the caller computes the
/// shortfall across all of the track's fragments and applies it only to the last one.
///
/// Only operates when trun carries per-sample duration (flags bit 0x100 set). Skips silently
/// if the format differs (e.g. default_sample_duration in tfhd), leaving the segment untouched.
fn extend_traf_last_sample(data: &mut [u8], traf_start: usize, traf_end: usize, shortfall_ticks: u64) {
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

            let last_dur_off = entry_base + (sample_count - 1) * stride;
            if last_dur_off + 4 <= traf_end {
                let old_dur = u32::from_be_bytes(
                    data[last_dur_off..last_dur_off + 4].try_into().unwrap_or([0; 4])
                ) as u64;
                let new_dur = ((old_dur + shortfall_ticks) as u32).to_be_bytes();
                data[last_dur_off..last_dur_off + 4].copy_from_slice(&new_dur);
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
