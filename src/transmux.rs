use anyhow::anyhow;
use bytes::Bytes;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Transmux MPEG-TS bytes to fragmented MP4 via ffmpeg.
/// Returns the full fMP4 output (ftyp + moov + moof + mdat).
pub async fn transmux_ts(ts_bytes: Bytes) -> anyhow::Result<Bytes> {
    let mut child = Command::new("ffmpeg")
        .args([
            "-loglevel", "error",
            "-i", "pipe:0",
            "-c", "copy",
            "-bsf:a", "aac_adtstoasc",
            "-movflags", "frag_keyframe+default_base_moof",
            "-f", "mp4",
            "pipe:1",
        ])
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
pub async fn transmux_ts_from_file(path: &str) -> anyhow::Result<Bytes> {
    let child = Command::new("ffmpeg")
        .args([
            "-loglevel", "error",
            "-allowed_extensions", "ALL",
            "-protocol_whitelist", "file,http,https,tcp,tls,crypto",
            "-i", path,
            "-c", "copy",
            "-bsf:a", "aac_adtstoasc",
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

/// Extract media segment — everything from the first `moof` box onwards.
pub fn extract_media(fmp4: &[u8]) -> Option<Bytes> {
    find_box_offset(fmp4, b"moof").map(|pos| Bytes::copy_from_slice(&fmp4[pos..]))
}

/// Return the byte offset of the first MP4 box with the given 4-byte type tag.
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
