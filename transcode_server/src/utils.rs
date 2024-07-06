use dotenv::{dotenv, var};

use base64::{engine::general_purpose, DecodeError, Engine as _};

use tonic::{transport::Server, Code, Request, Response, Status};

use serde::{Deserialize, Serialize};
use serde_json;

use std::error::Error;
use std::fs::metadata;
use std::fs::{File, OpenOptions};
use std::io::Write;

use tokio::fs;
use tokio::io::AsyncReadExt;

use sanitize_filename::sanitize;

use crate::s5::download_file;

pub fn bytes_to_base64url(bytes: &[u8]) -> String {
    let engine = general_purpose::STANDARD_NO_PAD;

    let mut base64_string = engine.encode(bytes);

    // Replace standard base64 characters with URL-safe ones
    base64_string = base64_string.replace("+", "-").replace("/", "_");

    base64_string
}

pub fn base64url_to_bytes(base64url: &str) -> Vec<u8> {
    let engine = general_purpose::STANDARD_NO_PAD;

    println!("base64url_to_bytes: base64url = {}", base64url);

    // Replace URL-safe characters with standard base64 ones
    let base64 = base64url
        .replace("-", "+")
        .replace("_", "/")
        .replace("=", "");

    engine.decode(&base64).unwrap()
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

    match download_file(url, file_path) {
        Ok(()) => println!("File downloaded successfully"),
        Err(e) => {
            eprintln!("Error downloading file: {}", e);
            return Err(Status::new(
                Code::Internal,
                format!("Error downloading file: {}", e),
            ));
        }
    }

    Ok(())
}

pub async fn download_and_concat_files(
    data: String,
    file_path: String,
) -> Result<(), Box<dyn Error>> {
    // Parse the JSON data
    let json_data: JsonData = serde_json::from_str(&data)?;

    // Open the final file
    let mut final_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file_path)
        .expect("Failed to open final_file");

    for (location_index, location) in json_data.locations.iter().enumerate() {
        let last_part_index = location.parts.len() - 1;
        for (part_index, part) in location.parts.iter().enumerate() {
            if location_index == json_data.locations.len() - 1 && part_index == last_part_index {
                continue;
            }

            println!("download_and_concat_files part: {}", part);

            let path_to_file = var("PATH_TO_FILE").unwrap();
            let tmp_file_path = String::from(path_to_file.to_owned() + &sanitize(part.as_str()));

            download_video(&part, tmp_file_path.as_str()).await?;

            let mut downloaded_file = match fs::File::open(&tmp_file_path).await {
                Ok(file) => file,
                Err(e) => {
                    eprintln!("Failed to open downloaded file {}: {}", &tmp_file_path, e);
                    continue;
                }
            };
            let mut buffer = Vec::new();
            downloaded_file.read_to_end(&mut buffer).await?;

            println!("Size of buffer: {}", buffer.len());

            // Append the content to the final file
            final_file.write_all(&buffer)?;

            let file_size = metadata(&file_path)?.len();
            println!("Size of final file: {} bytes", file_size);

            // Delete the downloaded file
            std::fs::remove_file(tmp_file_path)?;
        }
    }

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
