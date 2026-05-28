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

/// Extract init segment (ftyp + moov) — everything before the first `moof` box.
pub fn extract_init(fmp4: &[u8]) -> Option<Bytes> {
    find_box_offset(fmp4, b"moof").map(|pos| Bytes::copy_from_slice(&fmp4[..pos]))
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
