use crate::utils;

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose, Engine as _};
use dotenv::var;
use once_cell::sync::OnceCell;
use serde_json::Value;
use std::fs::File;
use std::io::copy;
use std::io::{BufReader, Read};
use std::process::Stdio;
use std::result::Result::{Err, Ok};
use std::str;
use std::time::Duration;
use std::{collections::HashMap, fs, path::Path};
use tokio::io::AsyncWriteExt;
use tus_client::Client;

use utils::bytes_to_base64url;

/// One client for every download and upload: it keeps connections open between
/// requests, and each new client would start its own thread.
///
/// Reaches no proxy from the environment, so that a stray `HTTP_PROXY` can't send
/// these requests, and the portal token, somewhere else. The timeout applies to
/// each request or read, so it limits a stalled transfer and not a long one.
fn http_client() -> Result<reqwest::blocking::Client> {
    static CLIENT: OnceCell<reqwest::blocking::Client> = OnceCell::new();
    let client = CLIENT.get_or_try_init(|| {
        reqwest::blocking::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(300))
            .build()
    })?;
    Ok(client.clone())
}

pub fn download_file(url: &str, path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let client = http_client()?;

    // An error status has a body too, and it isn't the file: never write that out.
    let mut response = client.get(url).send()?.error_for_status()?;

    // Save the response body to the specified file
    let mut file = File::create(path)?;
    if let Err(e) = copy(&mut response, &mut file) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(e.into());
    }

    Ok(())
}

pub async fn upload_video_s5(path: &str) -> Result<String, anyhow::Error> {
    // The tus client and the file hash both block, so keep them off the async workers.
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || upload_video_s5_blocking(&path))
        .await
        .map_err(|e| anyhow!("S5 upload task failed: {}", e))?
}

const UPLOAD_ATTEMPTS: u32 = 5;

fn upload_retry_pause(attempt: u32) -> Duration {
    if cfg!(test) {
        Duration::from_millis(1)
    } else {
        Duration::from_secs(2_u64.pow(attempt))
    }
}

/// Failures that trying again can't fix. A 4xx means the server has decided, apart
/// from 408 and 429, which ask for exactly that.
fn is_permanent(error: &tus_client::Error) -> bool {
    use tus_client::Error::*;
    match error {
        FileTooLarge | UnequalSizeError | FileReadError | IoError(_) => true,
        UnexpectedStatusCode(code) => (400..500).contains(code) && *code != 408 && *code != 429,
        _ => false,
    }
}

/// Which step of the upload an error belongs to.
enum UploadError {
    Create(tus_client::Error),
    Upload(tus_client::Error),
}

/// Creates the upload and sends the file, carrying on from where the server got to
/// after a failure: every attempt starts by asking for the current offset.
fn upload_with_retries(
    client: &Client,
    create_url: &str,
    path: &Path,
    metadata: &HashMap<String, String>,
    chunk_size: usize,
) -> std::result::Result<(), UploadError> {
    let mut upload_url: Option<String> = None;
    let mut attempt = 0;
    loop {
        let failure = match &upload_url {
            None => match client.create_with_metadata(create_url, path, metadata.clone()) {
                Ok(url) => {
                    println!("upload_url2 = {}", &url);
                    upload_url = Some(url);
                    continue;
                }
                Err(e) if is_permanent(&e) => return Err(UploadError::Create(e)),
                Err(e) => UploadError::Create(e),
            },
            Some(url) => match client.upload_with_chunk_size(url, path, chunk_size) {
                Ok(()) => return Ok(()),
                // The server has forgotten it, so all that's left is to start again.
                Err(tus_client::Error::NotFoundError) => {
                    upload_url = None;
                    UploadError::Upload(tus_client::Error::NotFoundError)
                }
                Err(e) if is_permanent(&e) => return Err(UploadError::Upload(e)),
                Err(e) => UploadError::Upload(e),
            },
        };

        attempt += 1;
        if attempt >= UPLOAD_ATTEMPTS {
            return Err(failure);
        }
        match &failure {
            UploadError::Create(e) | UploadError::Upload(e) => {
                eprintln!(
                    "Upload attempt {}/{} failed: {}",
                    attempt, UPLOAD_ATTEMPTS, e
                );
            }
        }
        std::thread::sleep(upload_retry_pause(attempt));
    }
}

fn upload_video_s5_blocking(path: &str) -> Result<String, anyhow::Error> {
    println!("upload_video_s5: path: {:?}", path);

    let portal_url = var("PORTAL_URL").map_err(|_| anyhow!("PORTAL_URL not set"))?;
    let token = var("TOKEN").map_err(|_| anyhow!("TOKEN not set"))?;

    let client = Client::new(http_client()?).with_auth_token(token);

    let path_str = path;
    let path = Path::new(path);
    let file_size = fs::metadata(path)
        .map_err(|e| anyhow!("Failed to read metadata for {}: {}", path_str, e))?
        .len();
    println!("file_size = {}", &file_size);

    let hash = hash_blake3_file(path_str.to_string())?;

    let mut metadata = HashMap::new();

    metadata.insert(
        String::from("hash"),
        general_purpose::URL_SAFE_NO_PAD.encode([&[31u8] as &[_], hash.as_bytes()].concat()),
    );

    let cid_bytes = utils::hash_bytes_to_cid(hash.as_bytes().to_vec(), file_size);
    println!("path = {}", &path.display());
    println!("portal_url = {}", &portal_url);

    let create_url = format!("{}{}", portal_url, "/s5/upload/tus");
    let chunk_size: usize = 1024 * 1024 * 5;
    match upload_with_retries(&client, &create_url, path, &metadata, chunk_size) {
        Ok(()) => (),
        Err(UploadError::Create(e)) => {
            return Err(anyhow!("Failed to create file on server: {}", e));
        }
        Err(UploadError::Upload(e)) => {
            return Err(anyhow!("Failed to upload file to server: {}", e));
        }
    }

    let cid = format!("u{}", bytes_to_base64url(&cid_bytes));
    Ok(cid)
}

const PINATA_URL: &str = "https://api.pinata.cloud/pinning/pinFileToIPFS";
const IPFS_UPLOAD_ATTEMPTS: u32 = 3;

fn pinata_setting(name: &str) -> Result<String> {
    std::env::var(name).map_err(|_| anyhow!("{} environment variable not set", name))
}

/// Sends a file with curl. The token doesn't appear here: it goes in through stdin
/// (`--config -`), because anything on a command line can be read with `ps`.
/// Gives up when a transfer stops moving, and however long a moving one takes.
fn curl_upload_command(path: &str, url: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("curl");
    cmd.args(["--silent", "--show-error"])
        .args(["--connect-timeout", "30"])
        .args(["--speed-limit", "1", "--speed-time", "300"])
        .args(["--config", "-"])
        .arg("--form")
        .arg(format!("file=@{}", path))
        .arg(url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

async fn upload_to_pinata_once(path: &str, url: &str, pinata_jwt: &str) -> Result<String> {
    let mut child = curl_upload_command(path, url)
        .spawn()
        .map_err(|e| anyhow!("Failed to execute curl command: {}", e))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("curl has no stdin"))?;
    stdin
        .write_all(format!("header = \"Authorization: Bearer {}\"\n", pinata_jwt).as_bytes())
        .await?;
    drop(stdin);

    let output = child
        .wait_with_output()
        .await
        .map_err(|e| anyhow!("Failed to wait for curl: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("curl command failed: {}", stderr));
    }

    let response_json: Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| anyhow!("Failed to parse JSON response from Pinata"))?;

    let cid = response_json["IpfsHash"]
        .as_str()
        .ok_or_else(|| anyhow!("IPFS hash not found in response"))?
        .to_string();

    println!("Extracted CID: {}", cid);

    Ok(cid)
}

pub async fn upload_video_ipfs(path: &str) -> Result<String, anyhow::Error> {
    let pinata_jwt = pinata_setting("PINATA_JWT")?;
    let url = pinata_setting("PINATA_API_URL").unwrap_or_else(|_| PINATA_URL.to_string());

    let mut attempt = 0;
    loop {
        attempt += 1;
        match upload_to_pinata_once(path, &url, &pinata_jwt).await {
            Ok(cid) => return Ok(cid),
            Err(e) if attempt >= IPFS_UPLOAD_ATTEMPTS => return Err(e),
            Err(e) => {
                eprintln!(
                    "IPFS upload attempt {}/{} failed: {}",
                    attempt, IPFS_UPLOAD_ATTEMPTS, e
                );
                tokio::time::sleep(upload_retry_pause(attempt)).await;
            }
        }
    }
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

    /// An error page isn't the file. It used to be saved as if it were.
    #[tokio::test]
    async fn test_download_refuses_an_http_error_and_writes_nothing() {
        let (base, _) = serve(|_, _| {
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot found"
                .to_vec()
        });
        let path = temp_path("error-page.bin");

        let result = crate::utils::download_video(&format!("{}/file", base), &path).await;

        assert!(result.is_err());
        assert!(!Path::new(&path).exists(), "no file for an error page");
    }

    #[tokio::test]
    async fn test_a_download_cut_off_part_way_is_an_error_and_leaves_no_file() {
        let (base, _) = serve(|_, _| {
            b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\nshort".to_vec()
        });
        let path = temp_path("cut-off.bin");

        let result = crate::utils::download_video(&format!("{}/file", base), &path).await;

        assert!(result.is_err());
        assert!(!Path::new(&path).exists(), "no half-written file");
    }

    // ── Uploads, against the shared fake portal ─────────────────────────────

    use crate::test_support::{portal, UploadFault, UPLOAD_TEST_LOCK};

    fn six_mib_file(name: &str) -> (String, Vec<u8>) {
        let file: Vec<u8> = (0..=255u8).cycle().take(6 * 1024 * 1024).collect();
        let path = temp_path(name);
        fs::write(&path, &file).unwrap();
        (path, file)
    }

    /// Drives the whole tus exchange over a real socket: create, read the offset,
    /// then PATCH in 5 MiB chunks.
    #[tokio::test]
    async fn test_upload_video_s5_sends_the_whole_file_when_called_from_async_code() {
        let portal = portal();
        let _uploads = UPLOAD_TEST_LOCK.lock().await;
        let (path, file) = six_mib_file("output.mp4");

        let cid = upload_video_s5(&path).await.unwrap();

        assert!(cid.starts_with('u'), "unexpected cid {}", cid);
        let uploads = portal.uploads();
        let upload = uploads.last().unwrap();
        assert_eq!(upload.length, 6291456);
        assert_eq!(upload.data, file);
    }

    /// The CID format is a contract with every client: 0x26 0x1f, the blake3 hash,
    /// then the size in little-endian with its trailing zero bytes dropped.
    #[tokio::test]
    async fn test_the_cid_is_built_from_the_hash_and_size_of_the_file() {
        portal();
        let _uploads = UPLOAD_TEST_LOCK.lock().await;
        let (path, file) = six_mib_file("hashed.mp4");
        let cid = upload_video_s5(&path).await.unwrap();

        let mut expected = vec![0x26, 0x1f];
        expected.extend_from_slice(blake3::hash(&file).as_bytes());
        expected.extend_from_slice(&[0x00, 0x00, 0x60]); // 6291456 = 0x600000
        assert_eq!(cid, format!("u{}", bytes_to_base64url(&expected)));
    }

    /// A network failure part way through must not cost the job its output. tus exists
    /// to pick up from where the server got to.
    #[tokio::test]
    async fn test_an_upload_carries_on_after_failed_chunks() {
        let portal = portal();
        let _uploads = UPLOAD_TEST_LOCK.lock().await;
        let (path, file) = six_mib_file("resumed.mp4");
        portal.fail_next_patches(vec![UploadFault::Status(503), UploadFault::HangUp]);

        upload_video_s5(&path).await.unwrap();

        assert_eq!(portal.uploads().last().unwrap().data, file);
    }

    /// A server is allowed to keep only part of a chunk. The next one has to start
    /// where the server says, or the stored file has a chunk in the wrong place.
    #[tokio::test]
    async fn test_a_chunk_the_server_only_partly_kept_is_resent_from_its_offset() {
        let portal = portal();
        let _uploads = UPLOAD_TEST_LOCK.lock().await;
        let (path, file) = six_mib_file("partial.mp4");
        portal.fail_next_patches(vec![UploadFault::KeepOnly(1024 * 1024)]);

        upload_video_s5(&path).await.unwrap();

        assert_eq!(portal.uploads().last().unwrap().data, file);
    }

    #[tokio::test]
    async fn test_creating_the_upload_is_retried() {
        let portal = portal();
        let _uploads = UPLOAD_TEST_LOCK.lock().await;
        let (path, file) = six_mib_file("create.mp4");
        portal.fail_next_creates(vec![UploadFault::Status(503), UploadFault::HangUp]);

        upload_video_s5(&path).await.unwrap();

        assert_eq!(portal.uploads().last().unwrap().data, file);
    }

    /// The server forgot the upload, as they do when one expires: start a new one.
    #[tokio::test]
    async fn test_an_upload_the_server_lost_is_started_again() {
        let portal = portal();
        let _uploads = UPLOAD_TEST_LOCK.lock().await;
        let (path, file) = six_mib_file("lost.mp4");
        let before = portal.uploads().len();
        portal.fail_next_patches(vec![UploadFault::Status(404)]);

        upload_video_s5(&path).await.unwrap();

        let uploads = portal.uploads();
        assert_eq!(uploads.len(), before + 2);
        assert_eq!(uploads.last().unwrap().data, file);
    }

    #[tokio::test]
    async fn test_an_upload_that_keeps_failing_gives_up_with_an_error() {
        let portal = portal();
        let _uploads = UPLOAD_TEST_LOCK.lock().await;
        let (path, _) = six_mib_file("fails.mp4");
        portal.fail_next_patches(vec![UploadFault::Status(500); UPLOAD_ATTEMPTS as usize]);

        let error = upload_video_s5(&path).await.unwrap_err();

        assert!(error.to_string().contains("Failed to upload"), "{}", error);
    }

    /// Being refused isn't going to change, and retrying would only delay the failure.
    #[tokio::test]
    async fn test_a_refused_token_is_not_retried() {
        let portal = portal();
        let _uploads = UPLOAD_TEST_LOCK.lock().await;
        let (path, _) = six_mib_file("refused.mp4");
        let before = portal.uploads().len();
        portal.fail_next_creates(vec![UploadFault::Status(401)]);

        let error = upload_video_s5(&path).await.unwrap_err();

        assert!(error.to_string().contains("Failed to create"), "{}", error);
        assert_eq!(portal.uploads().len(), before, "must not have tried again");
    }

    #[tokio::test]
    async fn test_uploading_a_missing_file_is_an_error_not_a_panic() {
        portal();
        assert!(upload_video_s5("/nonexistent/output.mp4").await.is_err());
    }

    /// Hash, size and token all arrive in the same request, so nothing about the token
    /// may have to leave this process another way.
    #[test]
    fn test_the_ipfs_token_is_not_on_the_command_line() {
        let cmd = curl_upload_command("/data/out.mp4", "https://example.test/pin");
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(!args.iter().any(|a| a.contains("Authorization")));
        assert!(args.contains(&"--config".to_string()));
    }

    /// The only test that uses PINATA_JWT and PINATA_API_URL.
    #[tokio::test]
    async fn test_ipfs_upload_retries_and_sends_its_token_as_a_header() {
        if std::process::Command::new("curl")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: needs curl");
            return;
        }
        let (base, seen) = serve(|_, earlier| {
            if earlier.is_empty() {
                return b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec();
            }
            let body = br#"{"IpfsHash":"QmTest"}"#;
            let mut reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            reply.extend_from_slice(body);
            reply
        });
        std::env::set_var("PINATA_JWT", "pinata-test-token");
        std::env::set_var("PINATA_API_URL", format!("{}/pin", base));
        let path = temp_path("ipfs.mp4");
        fs::write(&path, b"output bytes").unwrap();

        let cid = upload_video_ipfs(&path).await.unwrap();

        assert_eq!(cid, "QmTest");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "one failure, then the retry");
        assert!(seen[1]
            .head
            .contains("authorization: bearer pinata-test-token"));
        assert!(String::from_utf8_lossy(&seen[1].body).contains("output bytes"));
    }

    #[tokio::test]
    async fn test_ipfs_upload_without_a_token_is_an_error() {
        // Reads the token the same way as the real path, under a name that isn't set.
        assert!(pinata_setting("PINATA_NEVER_SET").is_err());
    }
}
