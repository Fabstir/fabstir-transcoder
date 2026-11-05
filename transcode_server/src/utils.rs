use anyhow::anyhow;

use base64::{engine::general_purpose, DecodeError, Engine as _};

use tonic::{Code, Status};

use serde::Deserialize;

use std::path::Path;
use std::time::Duration;

use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::s5::download_file;
use crate::shared::LocalFile;

pub fn bytes_to_base64url(bytes: &[u8]) -> String {
    let engine = general_purpose::STANDARD_NO_PAD;

    let mut base64_string = engine.encode(bytes);

    // Replace standard base64 characters with URL-safe ones
    base64_string = base64_string.replace("+", "-").replace("/", "_");

    base64_string
}

pub fn base64url_to_bytes(base64url: &str) -> Result<Vec<u8>, DecodeError> {
    let engine = general_purpose::STANDARD_NO_PAD;

    // Replace URL-safe characters with standard base64 ones
    let base64 = base64url
        .replace("-", "+")
        .replace("_", "/")
        .replace("=", "");

    engine.decode(&base64)
}

pub fn hash_bytes_to_cid(hash: Vec<u8>, file_size: u64) -> Vec<u8> {
    // Decode the base64url hash back to bytes
    // Prepend the byte 0x26 before the full hash
    let mut bytes = hash.to_vec();
    bytes.insert(0, 0x1f);
    bytes.insert(0, 0x26);

    // Append the size of the file, little-endian encoded
    let le_file_size = &file_size.to_le_bytes();
    let mut trimmed = le_file_size.as_slice();

    // Remove the trailing zeros
    while let Some(0) = trimmed.last() {
        trimmed = &trimmed[..trimmed.len() - 1];
    }

    bytes.extend(trimmed);

    bytes
}

/// Downloads a video from the specified `url` from S5 and saves it to disk. The
/// downloaded file is saved to the directory specified by the `PATH_TO_FILE`
/// environment variable, with a filename based on the URL. Returns the path
/// to the downloaded file as a `String`.
///
/// # Arguments
///
/// * `url` - The URL of the video to download.
///
pub async fn download_video(url: &str, file_path: &str) -> Result<(), Status> {
    println!(" {}", url);

    // The HTTP client is blocking, so keep it off the async workers: it would stall
    // every other task on the worker, and it panics there in debug builds.
    let (task_url, task_path) = (url.to_owned(), file_path.to_owned());
    let result = tokio::task::spawn_blocking(move || {
        download_file(&task_url, &task_path).map_err(|e| e.to_string())
    })
    .await;

    let message = match result {
        Ok(Ok(())) => {
            println!("File downloaded successfully");
            return Ok(());
        }
        Ok(Err(e)) => e,
        Err(e) => e.to_string(),
    };
    eprintln!("Error downloading file: {}", message);
    Err(Status::new(
        Code::Internal,
        format!("Error downloading file: {}", message),
    ))
}

const DOWNLOAD_ATTEMPTS: u32 = 3;

fn retry_pause(attempt: u32) -> Duration {
    if cfg!(test) {
        Duration::from_millis(1)
    } else {
        Duration::from_secs(2_u64.pow(attempt))
    }
}

/// Downloads `url` to `file_path`, trying again if that fails or finds nothing.
/// Afterwards the file is either there and not empty, or it isn't there at all.
pub async fn download_with_retries(url: &str, file_path: &str) -> Result<(), String> {
    let mut last_error = String::new();
    for attempt in 0..DOWNLOAD_ATTEMPTS {
        if attempt > 0 {
            println!(
                "Retrying download (attempt {}/{}): {}",
                attempt + 1,
                DOWNLOAD_ATTEMPTS,
                url
            );
            tokio::time::sleep(retry_pause(attempt)).await;
        }
        match download_video(url, file_path).await {
            Ok(()) => match fs::metadata(file_path).await {
                Ok(metadata) if metadata.len() > 0 => return Ok(()),
                Ok(_) => last_error = "the download was empty".to_string(),
                Err(e) => last_error = e.to_string(),
            },
            Err(e) => last_error = e.to_string(),
        }
    }
    let _ = fs::remove_file(file_path).await;
    Err(last_error)
}

/// Downloads every part listed in `data` and joins them into `file_path`. Any part
/// that can't be fetched fails the whole thing: what remains would be a shorter file
/// that nothing could tell from the real one.
pub async fn download_and_concat_files(data: String, file_path: String) -> anyhow::Result<()> {
    let json_data: JsonData = serde_json::from_str(&data)?;

    // Ensure we have at least one location with parts
    let parts = match json_data.locations.first() {
        Some(location) if !location.parts.is_empty() => &location.parts,
        _ => return Err(anyhow!("No file parts found in metadata")),
    };

    // Create parent directory if it doesn't exist
    if let Some(parent) = Path::new(&file_path).parent() {
        fs::create_dir_all(parent).await?;
    }

    let mut final_file = fs::File::create(&file_path).await?;

    // Get parts to download (all if only one exists, all except last if multiple exist)
    let content_parts = if parts.len() > 1 {
        &parts[..parts.len() - 1] // Skip last part only if multiple parts exist
    } else {
        &parts[..] // Use all parts if only one exists
    };

    let mut total_bytes_written = 0u64;
    for (index, part) in content_parts.iter().enumerate() {
        let downloaded = LocalFile::file(&format!("{}.{}.part", file_path, index));
        download_with_retries(part, downloaded.path())
            .await
            .map_err(|e| anyhow!("Failed to download part {}: {}", index, e))?;
        let mut part_file = fs::File::open(downloaded.path()).await?;
        total_bytes_written += tokio::io::copy(&mut part_file, &mut final_file).await?;
    }
    final_file.flush().await?;

    println!("Total bytes written: {}", total_bytes_written);
    Ok(())
}

#[derive(Debug, Deserialize)]
struct Location {
    parts: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct JsonData {
    locations: Vec<Location>,
}
