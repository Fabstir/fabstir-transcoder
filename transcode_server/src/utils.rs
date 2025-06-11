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
    
    // Get parts to download (all if only one exists, all except last if multiple exist)
    let parts = &json_data.locations[0].parts;
    let content_parts = if parts.len() > 1 {
        &parts[..parts.len()-1]  // Skip last part only if multiple parts exist
    } else {
        parts  // Use all parts if only one exists
    };
    
    let mut total_bytes_written = 0;
    const MAX_RETRIES: usize = 3;
    
    // Process each content part
    for part in content_parts {
        let path_to_file = var("PATH_TO_FILE").unwrap();
        let tmp_file_path = String::from(path_to_file.to_owned() + &sanitize(part.as_str()));
        
        let mut success = false;
        let mut retry_count = 0;
        
        // Retry loop for each part
        while !success && retry_count < MAX_RETRIES {
            if retry_count > 0 {
                println!("Retrying download (attempt {}/{}): {}", retry_count + 1, MAX_RETRIES, part);
                // Add exponential backoff delay
                tokio::time::sleep(std::time::Duration::from_millis(500 * 2_u64.pow(retry_count as u32))).await;
            }
            
            match download_video(&part, tmp_file_path.as_str()).await {
                Ok(_) => {
                    // Verify the downloaded file has content
                    match fs::metadata(&tmp_file_path).await {
                        Ok(metadata) => {
                            let file_size = metadata.len();
                            println!("Downloaded part size: {} bytes", file_size);
                            
                            if file_size == 0 {
                                println!("Warning: Downloaded file is empty, retrying...");
                                retry_count += 1;
                                continue;
                            }
                            
                            // Read and append file content
                            match fs::File::open(&tmp_file_path).await {
                                Ok(mut downloaded_file) => {
                                    let mut buffer = Vec::new();
                                    if let Ok(bytes_read) = downloaded_file.read_to_end(&mut buffer).await {
                                        if bytes_read > 0 {
                                            match final_file.write_all(&buffer) {
                                                Ok(_) => {
                                                    total_bytes_written += bytes_read;
                                                    success = true;
                                                    println!("Successfully appended {} bytes", bytes_read);
                                                },
                                                Err(e) => {
                                                    eprintln!("Failed to write to final file: {}", e);
                                                    retry_count += 1;
                                                }
                                            }
                                        } else {
                                            println!("Warning: Read 0 bytes from downloaded file, retrying...");
                                            retry_count += 1;
                                        }
                                    } else {
                                        eprintln!("Failed to read downloaded file");
                                        retry_count += 1;
                                    }
                                },
                                Err(e) => {
                                    eprintln!("Failed to open downloaded file: {}", e);
                                    retry_count += 1;
                                }
                            }
                        },
                        Err(e) => {
                            eprintln!("Failed to get metadata for downloaded file: {}", e);
                            retry_count += 1;
                        }
                    }
                },
                Err(e) => {
                    eprintln!("Download error: {}", e);
                    retry_count += 1;
                }
            }
            
            // Clean up regardless of success
            if std::path::Path::new(&tmp_file_path).exists() {
                let _ = std::fs::remove_file(&tmp_file_path);
            }
        }
        
        if !success {
            return Err(format!("Failed to download part after {} retries: {}", MAX_RETRIES, part).into());
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
