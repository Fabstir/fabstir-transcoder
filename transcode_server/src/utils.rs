use dotenv::{dotenv, var};

use base64::{engine::general_purpose, DecodeError, Engine as _};

use tonic::{transport::Server, Code, Request, Response, Status};

use serde::{Deserialize, Serialize};
use serde_json;

use std::error::Error;
use std::fs::metadata;
use std::fs::{File, OpenOptions};
use std::path::Path;
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
    println!("download_and_concat_files - metadata: {}", data);
    
    // Parse the JSON data
    let json_data: JsonData = serde_json::from_str(&data)?;
    
    // Ensure we have at least one location with parts
    if json_data.locations.is_empty() || json_data.locations[0].parts.is_empty() {
        return Err("No file parts found in metadata".into());
    }
    
    // Create parent directory if it doesn't exist
    if let Some(parent) = Path::new(&file_path).parent() {
        fs::create_dir_all(parent).await?;
    }

    // Open the final file
    let mut final_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&file_path)
        .expect("Failed to open final_file");
    
    let mut total_bytes_written = 0;
    
    // Process each location
    'location_loop: for (location_index, location) in json_data.locations.iter().enumerate() {
        println!("Processing location {}: {} parts", location_index, location.parts.len());
        
        // Try each part in this location
        for (part_index, part) in location.parts.iter().enumerate() {
            // Skip obviously invalid URLs
            if part.len() < 10 || !part.contains('/') {
                println!("Skipping invalid URL: {}", part);
                continue;
            }
            
            // More strict URL validation - must contain domain and path
            let url_parts: Vec<&str> = part.split('/').collect();
            if url_parts.len() < 4 || url_parts[3].is_empty() {
                // URL should have at least protocol, empty string, domain, and some path
                // e.g., "https://example.com/path" splits into ["https:", "", "example.com", "path"]
                println!("Skipping URL with no file path: {}", part);
                continue;
            }
            
            println!("Trying to download part {}: {}", part_index, part);
            
            // Create a temporary file path
            let path_to_file = var("PATH_TO_FILE").unwrap_or_else(|_| "./tmp/".to_string());
            let tmp_file_path = format!("{}{}", path_to_file, sanitize(&format!("part_{}", part_index)));
            
            // Try to download the file
            if let Ok(_) = download_video(&part, &tmp_file_path).await {
                // Check if the file was downloaded successfully
                if let Ok(metadata) = fs::metadata(&tmp_file_path).await {
                    let file_size = metadata.len();
                    println!("Downloaded part size: {} bytes", file_size);
                    
                    if file_size > 0 {
                        // Read and append the file content
                        if let Ok(mut downloaded_file) = fs::File::open(&tmp_file_path).await {
                            let mut buffer = Vec::new();
                            if let Ok(bytes_read) = downloaded_file.read_to_end(&mut buffer).await {
                                if bytes_read > 0 {
                                    if final_file.write_all(&buffer).is_ok() {
                                        total_bytes_written += bytes_read;
                                        println!("Successfully appended {} bytes", bytes_read);
                                    }
                                }
                            }
                        }
                    }
                }
                
                // Clean up temporary file
                let _ = std::fs::remove_file(&tmp_file_path);
                
                // If we successfully downloaded and processed a part, move to the next location
                if total_bytes_written > 0 {
                    println!("Successfully downloaded content from location {}", location_index);
                    continue 'location_loop;
                }
            }
        }
    }
    
    // Final verification
    if total_bytes_written == 0 {
        return Err("No data was written to the output file".into());
    }
    
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
