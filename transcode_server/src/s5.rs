use crate::utils;

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose, Engine as _};
use dotenv::var;
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
    //    // Print the values of the input variables
    //    println!("download_file: Downloading file from URL: {}", url);
    //    println!("download_file: Saving file to path: {}", path);

    // Create a new client with default configuration
    let client = reqwest::blocking::Client::new();

    // Send a GET request to the download URL
    let mut response = client.get(url).send()?;

    // Save the response body to the specified file
    let mut file = File::create(path)?;
    copy(&mut response, &mut file)?;

    Ok(())
}

pub async fn upload_video_s5(path: &str) -> Result<String, anyhow::Error> {
    // The tus client and the file hash both block, so keep them off the async workers.
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || upload_video_s5_blocking(&path))
        .await
        .map_err(|e| anyhow!("S5 upload task failed: {}", e))?
}

fn upload_video_s5_blocking(path: &str) -> Result<String, anyhow::Error> {
    println!("upload_video_s5: path: {:?}", path);

    let portal_url = var("PORTAL_URL").unwrap();
    let token = var("TOKEN").unwrap();

    let client = Client::new(reqwest::blocking::Client::new()).with_auth_token(token);

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
            return Err(anyhow!("Failed to create file on server: {}", e));
        }
    };

    println!("upload_url2 = {}", &upload_url);
    let chunk_size: usize = 1024 * 1024 * 5;
    match client.upload_with_chunk_size(&upload_url, path, chunk_size) {
        Ok(_) => (),
        Err(e) => {
            return Err(anyhow!("Failed to upload file to server: {}", e));
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone)]
    struct Seen {
        method: String,
        head: String,
        body: Vec<u8>,
    }

    fn read_request(stream: &mut TcpStream) -> Seen {
        let mut data = Vec::new();
        let mut buf = [0u8; 65536];
        let head_end = loop {
            if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            let n = stream.read(&mut buf).unwrap();
            assert!(n > 0, "client closed before sending headers");
            data.extend_from_slice(&buf[..n]);
        };
        let head = String::from_utf8_lossy(&data[..head_end]).to_lowercase();
        let length: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .map_or(0, |v| v.trim().parse().unwrap());
        while data.len() < head_end + length {
            let n = stream.read(&mut buf).unwrap();
            assert!(n > 0, "client closed before sending the body");
            data.extend_from_slice(&buf[..n]);
        }
        Seen {
            method: head.split(' ').next().unwrap().to_uppercase(),
            body: data[head_end..].to_vec(),
            head,
        }
    }

    /// A real HTTP server on a real socket, one request per connection, so these
    /// tests exercise the HTTP client and not a stand-in for it.
    fn serve<F>(respond: F) -> (String, Arc<Mutex<Vec<Seen>>>)
    where
        F: Fn(&Seen, &[Seen]) -> Vec<u8> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let request = read_request(&mut stream);
                let mut log = log.lock().unwrap();
                let reply = respond(&request, &log);
                log.push(request);
                drop(log);
                stream.write_all(&reply).unwrap();
            }
        });
        (base, seen)
    }

    fn temp_path(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("s5-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir.join(name).to_str().unwrap().to_string()
    }

    /// The blocking HTTP client must never run on an async worker: besides
    /// stalling every other task on that worker, it panics there in debug builds.
    #[tokio::test]
    async fn test_download_video_saves_the_body_when_called_from_async_code() {
        let body: Vec<u8> = (0..=255u8).cycle().take(300_000).collect();
        let expected = body.clone();
        let (base, _) = serve(move |_, _| {
            let mut reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            reply.extend_from_slice(&body);
            reply
        });
        let path = temp_path("source.bin");

        crate::utils::download_video(&format!("{}/file", base), &path)
            .await
            .unwrap();

        assert_eq!(fs::read(&path).unwrap(), expected);
    }

    #[tokio::test]
    async fn test_download_video_reports_a_connection_failure_as_an_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let path = temp_path("never.bin");

        let result = crate::utils::download_video(&format!("http://{}/file", addr), &path).await;

        assert!(result.is_err());
    }

    /// Drives the whole tus exchange over a real socket: create, read the offset,
    /// then PATCH in 5 MiB chunks. The only test that uses PORTAL_URL and TOKEN.
    #[tokio::test]
    async fn test_upload_video_s5_sends_the_whole_file_when_called_from_async_code() {
        let file: Vec<u8> = (0..=255u8).cycle().take(6 * 1024 * 1024).collect();
        let total = file.len();
        let (base, seen) = serve(move |request, earlier| {
            let uploaded: usize = earlier
                .iter()
                .filter(|r| r.method == "PATCH")
                .map(|r| r.body.len())
                .sum();
            let reply = match request.method.as_str() {
                "POST" => {
                    let host = request
                        .head
                        .lines()
                        .find_map(|l| l.strip_prefix("host: "))
                        .unwrap();
                    format!(
                        "HTTP/1.1 201 Created\r\nLocation: http://{}/upload/1\r\n\
                         Content-Length: 0\r\nConnection: close\r\n\r\n",
                        host
                    )
                }
                "HEAD" => format!(
                    "HTTP/1.1 200 OK\r\nUpload-Offset: 0\r\nUpload-Length: {}\r\n\
                     Content-Length: 0\r\nConnection: close\r\n\r\n",
                    total
                ),
                "PATCH" => format!(
                    "HTTP/1.1 204 No Content\r\nUpload-Offset: {}\r\nConnection: close\r\n\r\n",
                    uploaded + request.body.len()
                ),
                other => panic!("unexpected method {}", other),
            };
            reply.into_bytes()
        });
        std::env::set_var("PORTAL_URL", &base);
        std::env::set_var("TOKEN", "test-token");
        let path = temp_path("output.mp4");
        fs::write(&path, &file).unwrap();

        let result = upload_video_s5(&path).await;

        let cid = result.unwrap();
        assert!(cid.starts_with('u'), "unexpected cid {}", cid);
        let seen = seen.lock().unwrap();
        let methods: Vec<&str> = seen.iter().map(|r| r.method.as_str()).collect();
        assert_eq!(methods, ["POST", "HEAD", "PATCH", "PATCH"]);
        let sent: Vec<u8> = seen.iter().flat_map(|r| r.body.clone()).collect();
        assert_eq!(sent, file);
        assert!(seen[0].head.contains("upload-length: 6291456"));
        assert!(seen[0].head.contains("test-token"));
    }
}
