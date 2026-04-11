use anyhow::{anyhow, Result};
use serde::Serialize;
use std::fs;
use std::sync::Arc;
use tokio::sync::Semaphore;

use crate::encrypt_file::encrypt_file_xchacha20;
use crate::encrypted_cid::create_encrypted_cid;
use crate::s5::{hash_blake3_file, upload_video};
use crate::shared;
use crate::utils::{bytes_to_base64url, hash_bytes_to_cid};

#[derive(Debug, Clone, Serialize)]
pub struct HlsSegmentInfo {
    pub index: u32,
    pub cid: String,
    pub duration: f64,
    pub encrypted: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct HlsFormatResult {
    pub init_segment_cid: String,
    pub segments: Vec<HlsSegmentInfo>,
    pub preview_segments: u32,
    pub total_segments: u32,
    pub total_duration: f64,
}

#[derive(Debug)]
pub struct PlaylistParseResult {
    pub init_segment: String,
    pub segments: Vec<(String, f64)>,
}

pub fn parse_hls_playlist(path: &str) -> Result<PlaylistParseResult> {
    let content =
        fs::read_to_string(path).map_err(|e| anyhow!("Failed to read playlist {}: {}", path, e))?;

    let mut init_segment: Option<String> = None;
    let mut segments: Vec<(String, f64)> = Vec::new();
    let mut pending_duration: Option<f64> = None;

    for line in content.lines() {
        let line = line.trim();
        if line.starts_with("#EXT-X-MAP:") {
            if let Some(start) = line.find("URI=\"") {
                let rest = &line[start + 5..];
                if let Some(end) = rest.find('"') {
                    init_segment = Some(rest[..end].to_string());
                }
            }
        } else if line.starts_with("#EXTINF:") {
            let dur_str = line.trim_start_matches("#EXTINF:");
            let dur_str = dur_str.trim_end_matches(',');
            if let Ok(dur) = dur_str.parse::<f64>() {
                pending_duration = Some(dur);
            }
        } else if !line.starts_with('#') && !line.is_empty() {
            if let Some(dur) = pending_duration.take() {
                segments.push((line.to_string(), dur));
            }
        }
    }

    let init_segment = init_segment.ok_or_else(|| anyhow!("No #EXT-X-MAP found in playlist"))?;
    if segments.is_empty() {
        return Err(anyhow!("No segments found in playlist"));
    }

    Ok(PlaylistParseResult {
        init_segment,
        segments,
    })
}

pub fn compute_preview_boundary(total_segments: u32, preview_percent: u32) -> u32 {
    if preview_percent == 0 || preview_percent > 100 {
        return 0;
    }
    (total_segments as u64 * preview_percent as u64 / 100) as u32
}

fn prefix_cid(cid: &str, dest: &Option<String>) -> String {
    match dest.as_deref() {
        Some("ipfs") => format!("ipfs://{}", cid),
        _ => format!("s5://{}", cid),
    }
}

pub async fn upload_single_segment(
    path: &str,
    _index: u32,
    should_encrypt: bool,
    dest: Option<String>,
) -> Result<(String, bool)> {
    if !should_encrypt {
        let cid = upload_video(path, dest.clone())
            .await
            .map_err(|e| anyhow!("Upload failed for {}: {}", path, e))?;
        return Ok((prefix_cid(&cid, &dest), false));
    }

    // Encrypted path
    let encrypted_path = format!("{}_enc", path);
    let path_owned = path.to_string();
    let encrypted_path_clone = encrypted_path.clone();

    // CPU-bound: encrypt + hash in spawn_blocking
    let (key, hash_orig, hash_enc) =
        tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, blake3::Hash, blake3::Hash)> {
            let key = encrypt_file_xchacha20(path_owned.clone(), encrypted_path_clone.clone(), 0)
                .map_err(|e| anyhow!("Encryption failed: {}", e))?;

            let hash_orig =
                hash_blake3_file(path_owned).map_err(|e| anyhow!("Hash failed: {}", e))?;
            let hash_enc = hash_blake3_file(encrypted_path_clone)
                .map_err(|e| anyhow!("Hash failed: {}", e))?;

            Ok((key, hash_orig, hash_enc))
        })
        .await
        .map_err(|e| anyhow!("spawn_blocking failed: {}", e))??;

    // Async: upload encrypted file
    let _upload_cid = upload_video(&encrypted_path, dest.clone())
        .await
        .map_err(|e| anyhow!("Upload failed for {}: {}", encrypted_path, e))?;

    // Build encrypted CID
    let mut encrypted_blob_hash = vec![0x1f];
    encrypted_blob_hash.extend(hash_enc.as_bytes());

    let file_size = fs::metadata(path)
        .map_err(|e| anyhow!("Failed to read metadata: {}", e))?
        .len();
    let original_cid = hash_bytes_to_cid(hash_orig.as_bytes().to_vec(), file_size);

    let encrypted_cid_bytes =
        create_encrypted_cid(0xae, 0xa6, 18, encrypted_blob_hash, key, 0, original_cid);

    let encrypted_cid = format!("s5://u{}", bytes_to_base64url(&encrypted_cid_bytes));

    // Clean up encrypted file
    fs::remove_file(&encrypted_path).ok();

    Ok((encrypted_cid, true))
}

const MAX_CONCURRENT_UPLOADS: usize = 10;

pub async fn process_hls_segments(
    task_id: &str,
    format_index: usize,
    hls_dir: &str,
    preview_percent: u32,
    dest: Option<String>,
) -> Result<HlsFormatResult> {
    let playlist_path = format!("{}/playlist.m3u8", hls_dir);
    let parsed = parse_hls_playlist(&playlist_path)?;

    let total_segments = parsed.segments.len() as u32;
    let preview_boundary = compute_preview_boundary(total_segments, preview_percent);

    // Upload init segment (always unencrypted)
    let init_path = format!("{}/{}", hls_dir, parsed.init_segment);
    let init_cid = upload_video(&init_path, dest.clone())
        .await
        .map_err(|e| anyhow!("Init segment upload failed: {}", e))?;
    let init_segment_cid = prefix_cid(&init_cid, &dest);

    // Parallel segment uploads with semaphore
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_UPLOADS));
    let mut handles = Vec::new();

    for (i, (filename, duration)) in parsed.segments.iter().enumerate() {
        let sem = Arc::clone(&semaphore);
        let seg_path = format!("{}/{}", hls_dir, filename);
        let should_encrypt = (i as u32) >= preview_boundary;
        let dest_clone = dest.clone();
        let dur = *duration;
        let idx = i as u32;

        let handle = tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            let result = upload_single_segment(&seg_path, idx, should_encrypt, dest_clone).await;
            result.map(|(cid, encrypted)| HlsSegmentInfo {
                index: idx,
                cid,
                duration: dur,
                encrypted,
            })
        });
        handles.push(handle);
    }

    // Collect results
    let mut segments: Vec<HlsSegmentInfo> = Vec::new();
    let total = handles.len();
    for (done, handle) in handles.into_iter().enumerate() {
        let info = handle
            .await
            .map_err(|e| anyhow!("Segment task failed: {}", e))??;
        segments.push(info);
        let progress = 70 + ((done + 1) * 30 / total) as i32;
        shared::update_progress(task_id, format_index, progress);
    }

    segments.sort_by_key(|s| s.index);

    let total_duration: f64 = segments.iter().map(|s| s.duration).sum();

    // Cleanup HLS directory
    fs::remove_dir_all(hls_dir).ok();

    Ok(HlsFormatResult {
        init_segment_cid,
        segments,
        preview_segments: preview_boundary,
        total_segments,
        total_duration,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_test_playlist(path: &str, content: &str) {
        let mut f = fs::File::create(path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn test_parse_hls_playlist_basic() {
        let dir = std::env::temp_dir().join("hls_test_basic");
        fs::create_dir_all(&dir).ok();
        let path = dir.join("playlist.m3u8");
        write_test_playlist(
            path.to_str().unwrap(),
            "\
#EXTM3U
#EXT-X-VERSION:7
#EXT-X-TARGETDURATION:6
#EXT-X-MAP:URI=\"init.mp4\"
#EXTINF:6.006000,
seg_0000.m4s
#EXTINF:6.006000,
seg_0001.m4s
#EXTINF:3.500000,
seg_0002.m4s
#EXT-X-ENDLIST
",
        );
        let result = parse_hls_playlist(path.to_str().unwrap()).unwrap();
        assert_eq!(result.init_segment, "init.mp4");
        assert_eq!(result.segments.len(), 3);
        assert_eq!(result.segments[0].0, "seg_0000.m4s");
        assert_eq!(result.segments[1].0, "seg_0001.m4s");
        assert_eq!(result.segments[2].0, "seg_0002.m4s");
        assert!((result.segments[0].1 - 6.006).abs() < 0.001);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_parse_hls_playlist_last_segment_short() {
        let dir = std::env::temp_dir().join("hls_test_short");
        fs::create_dir_all(&dir).ok();
        let path = dir.join("playlist.m3u8");
        write_test_playlist(
            path.to_str().unwrap(),
            "\
#EXTM3U
#EXT-X-VERSION:7
#EXT-X-TARGETDURATION:6
#EXT-X-MAP:URI=\"init.mp4\"
#EXTINF:6.006000,
seg_0000.m4s
#EXTINF:3.500000,
seg_0001.m4s
#EXT-X-ENDLIST
",
        );
        let result = parse_hls_playlist(path.to_str().unwrap()).unwrap();
        assert!((result.segments[1].1 - 3.5).abs() < 0.001);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_compute_preview_boundary_cases() {
        assert_eq!(compute_preview_boundary(100, 15), 15);
        assert_eq!(compute_preview_boundary(100, 0), 0);
        assert_eq!(compute_preview_boundary(100, 100), 100);
        assert_eq!(compute_preview_boundary(10, 15), 1);
        assert_eq!(compute_preview_boundary(1, 50), 0);
        assert_eq!(compute_preview_boundary(200, 10), 20);
    }

    #[test]
    fn test_compute_preview_boundary_edge_zero_total() {
        assert_eq!(compute_preview_boundary(0, 15), 0);
    }

    #[test]
    fn test_compute_preview_boundary_over_100() {
        assert_eq!(compute_preview_boundary(100, 150), 0);
    }
}
