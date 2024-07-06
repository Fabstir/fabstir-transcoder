use crate::utils;

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose, Engine as _};
use dotenv::var;
use reqwest::multipart;
use serde_json::Value;
use std::env;
use std::fs::File;
use std::io::copy;
use std::io::{BufReader, Read};
use std::process::Command;
use std::result::Result::{Err, Ok};
use std::str;
use std::{collections::HashMap, fs, path::Path};
use tokio::io::AsyncReadExt;
use tokio::runtime::Runtime;
use tus_client::Client;

use utils::bytes_to_base64url;

pub fn download_file(url: &str, path: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Create a new client with default configuration
    let client = reqwest::Client::new();

    // Send a GET request to the download URL
    let mut response = client.get(url).send()?;

    // Save the response body to the specified file
    let mut file = File::create(path)?;
    copy(&mut response, &mut file)?;

    Ok(())
}

pub async fn upload_video_s5(path: &str) -> Result<String, anyhow::Error> {
    println!("upload_video_s5: path: {:?}", path);

    let portal_url = var("PORTAL_URL").unwrap();
    let token = var("TOKEN").unwrap();

    let client = Client::new(reqwest::Client::new()).with_auth_token(token);

    let path = Path::new(path);
    let metadata = fs::metadata(path).expect("Failed to read metadata");
    let file_size = metadata.len();
    println!("file_size = {}", &file_size);

    let hash = hash_blake3_file(String::from(path.to_str().unwrap())).unwrap();

    let mut metadata = HashMap::new();

    metadata.insert(
        String::from("hash"),
        general_purpose::URL_SAFE_NO_PAD.encode([&[31u8] as &[_], hash.as_bytes()].concat()),
    );

    println!("{}", metadata.get("hash").unwrap());

    let cid_bytes = hash_to_cid(metadata.get("hash").unwrap(), file_size);
    println!("cid = {:?}", cid_bytes);
    println!("path = {}", &path.display());
    println!("portal_url = {}", &portal_url);
    println!("metadata = {:?}", metadata);

    let upload_url = match client.create_with_metadata(
        &format!("{}{}", portal_url, "/s5/upload/tus"),
        path,
        metadata,
    ) {
        Ok(url) => url,
        Err(e) => {
            eprintln!("Failed to create file on server: {}", e);
            String::new()
        }
    };

    println!("upload_url2 = {}", &upload_url);
    let chunk_size: usize = 1024 * 1024 * 5;
    match client.upload_with_chunk_size(&upload_url, path, chunk_size) {
        Ok(_) => (),
        Err(e) => eprintln!("Failed to upload file to server: {}", e),
    }

    println!("upload_video_s5: cid: {:?}", cid_bytes);

    let cid = format!("u{}", bytes_to_base64url(&cid_bytes));
    Ok(cid)
}

pub async fn upload_video_ipfs(path: &str) -> Result<String, anyhow::Error> {
    let pinata_jwt = std::env::var("PINATA_JWT")
        .map_err(|_| anyhow!("PINATA_JWT environment variable not set"))?;

    // Using `curl` to upload the file
    let output = Command::new("curl")
        .arg("-X")
        .arg("POST")
        .arg("--header")
        .arg(format!("Authorization: Bearer {}", pinata_jwt))
        .arg("--form")
        .arg(format!("file=@{}", path))
        .arg("https://api.pinata.cloud/pinning/pinFileToIPFS")
        .output()
        .map_err(|e| anyhow!("Failed to execute curl command: {}", e))?;

    if !output.status.success() {
        let stderr = str::from_utf8(&output.stderr).unwrap_or("Failed to read stderr");
        return Err(anyhow!("curl command failed: {}", stderr));
    }

    let response_body = str::from_utf8(&output.stdout)
        .map_err(|_| anyhow!("Failed to read curl command output"))?;

    // Debugging: Print the response body
    println!("Curl response body: {}", response_body);

    let response_json: Value = serde_json::from_str(response_body)
        .map_err(|_| anyhow!("Failed to parse JSON response from Pinata"))?;

    let cid_bytes = response_json["IpfsHash"]
        .as_str()
        .ok_or_else(|| anyhow!("IPFS hash not found in response"))?
        .as_bytes()
        .to_vec();

    let cid = String::from_utf8(cid_bytes)
        .map_err(|_| anyhow!("Failed to convert CID bytes to string"))?;

    // Debugging: Print the CID
    println!("Extracted CID: {}", cid);

    Ok(cid)
}

pub async fn upload_video(
    path: &str,
    storage_network: Option<String>,
) -> Result<String, anyhow::Error> {
    match storage_network.as_deref() {
        Some("ipfs") => upload_video_ipfs(path).await,
        _ => upload_video_s5(path).await,
    }
}

pub fn hash_blake3_file(path: String) -> Result<blake3::Hash, anyhow::Error> {
    let input = File::open(path)?;
    let reader = BufReader::new(input);
    let digest = blake3_digest(reader)?;

    Ok(digest)
}

fn blake3_digest<R: Read>(mut reader: R) -> Result<blake3::Hash, anyhow::Error> {
    let mut hasher = blake3::Hasher::new();

    let mut buffer = [0; 1048576];

    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }

    Ok(hasher.finalize())
}

pub fn hash_to_cid(hash: &str, file_size: u64) -> Vec<u8> {
    // Decode the base64url hash back to bytes
    let cid = general_purpose::URL_SAFE_NO_PAD.decode(hash).unwrap();

    // Clone the CID to a mutable vector of bytes
    let mut bytes = cid.to_vec();

    // Prepend the byte 0x26 before the full hash
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
