//! Shared by the tests of several modules: temporary storage, a fake S5 portal on a
//! real socket, and a small real video. The job path reads its configuration from
//! the process environment once, so all of them share one set-up.

use once_cell::sync::Lazy;
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

pub struct Storage {
    pub sources: String,
    pub outputs: String,
}

static STORAGE: Lazy<Storage> = Lazy::new(|| {
    let root = std::env::temp_dir().join(format!("transcoder-test-{}", uuid::Uuid::new_v4()));
    let sources = format!("{}/sources/", root.display());
    let outputs = format!("{}/outputs/", root.display());
    std::fs::create_dir_all(&sources).unwrap();
    std::fs::create_dir_all(&outputs).unwrap();
    std::env::set_var("PATH_TO_FILE", &sources);
    std::env::set_var("PATH_TO_TRANSCODED_FILE", &outputs);
    Storage { sources, outputs }
});

/// Points the job path at temporary directories.
pub fn storage() -> &'static Storage {
    &STORAGE
}

/// Names in the output directory containing `needle`.
pub fn outputs_containing(needle: &str) -> Vec<String> {
    std::fs::read_dir(&storage().outputs)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.contains(needle))
        .collect()
}

/// Names in the source directory containing `needle`.
pub fn sources_containing(needle: &str) -> Vec<String> {
    std::fs::read_dir(&storage().sources)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.contains(needle))
        .collect()
}

/// A CID no other test is using, so each test gets its own cache entry.
pub fn fresh_cid() -> String {
    format!("cid{}", uuid::Uuid::new_v4().simple())
}

// ── A real video ────────────────────────────────────────────────────────────

/// False where FFmpeg with the free encoders isn't installed; those tests skip.
pub fn have_ffmpeg() -> bool {
    static HAVE: Lazy<bool> = Lazy::new(|| {
        let encoders = Command::new("ffmpeg")
            .args(["-hide_banner", "-encoders"])
            .stderr(Stdio::null())
            .output();
        let probe = Command::new("ffprobe")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let usable = matches!(&encoders, Ok(out)
            if String::from_utf8_lossy(&out.stdout).contains("libx264"));
        usable && matches!(probe, Ok(status) if status.success())
    });
    if !*HAVE {
        eprintln!("skipping: needs ffmpeg with libx264, and ffprobe");
    }
    *HAVE
}

fn generate(args: &[&str], ext: &str) -> Vec<u8> {
    let path = std::env::temp_dir().join(format!("sample-{}.{}", uuid::Uuid::new_v4(), ext));
    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin"])
        .args(args)
        .args(["-y", path.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success(), "could not generate a test sample");
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    bytes
}

/// Three seconds of 64x64 video with sound.
pub fn sample_video() -> &'static [u8] {
    static SAMPLE: Lazy<Vec<u8>> = Lazy::new(|| {
        generate(
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=3:size=64x64:rate=10",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=3",
                "-c:v",
                "libx264",
                "-c:a",
                "aac",
                "-shortest",
            ],
            "mp4",
        )
    });
    &SAMPLE
}

/// Three seconds of sound with no video stream.
pub fn sample_audio() -> &'static [u8] {
    static SAMPLE: Lazy<Vec<u8>> = Lazy::new(|| {
        generate(
            &[
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=3",
                "-c:a",
                "aac",
            ],
            "m4a",
        )
    });
    &SAMPLE
}

/// A 120 second MKV whose header claims it lasts 4 seconds.
pub fn sample_with_a_false_duration() -> Vec<u8> {
    let mut bytes = generate(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=120:size=64x64:rate=2",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-g",
            "2",
            // A fixed spot to overwrite: the first Duration element in the file.
            "-metadata",
            "title=x",
        ],
        "mkv",
    );
    // Matroska Duration element: ID 0x4489, size 0x88, then a big-endian float64
    // holding the length in timestamp units (milliseconds by default).
    let at = bytes
        .windows(3)
        .position(|w| w == [0x44, 0x89, 0x88])
        .expect("Duration element");
    bytes[at + 3..at + 11].copy_from_slice(&4000f64.to_be_bytes());
    bytes
}

// ── A fake S5 portal ────────────────────────────────────────────────────────

/// A canned reply used once, ahead of whatever the path normally returns.
#[derive(Clone)]
pub enum Fault {
    Status(u16),
    /// Promise more bytes than get sent, then hang up.
    CutOff,
}

/// What goes wrong with an upload request.
#[derive(Clone)]
pub enum UploadFault {
    Status(u16),
    HangUp,
    /// Answer 204, having kept only this many bytes of the chunk.
    KeepOnly(usize),
}

#[derive(Default, Clone)]
pub struct Upload {
    pub length: usize,
    pub data: Vec<u8>,
}

#[derive(Default)]
struct State {
    blobs: HashMap<String, Vec<u8>>,
    faults: HashMap<String, VecDeque<Fault>>,
    requests: Vec<(String, String)>,
    uploads: Vec<Upload>,
    create_faults: VecDeque<UploadFault>,
    patch_faults: VecDeque<UploadFault>,
}

pub struct Portal {
    pub url: String,
    state: Arc<Mutex<State>>,
}

struct Request {
    method: String,
    path: String,
    headers: String,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut data = Vec::new();
    let mut buf = [0u8; 65536];
    let head_end = loop {
        if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        data.extend_from_slice(&buf[..n]);
    };
    let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
    let lower = head.to_lowercase();
    let length: usize = lower
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .map_or(0, |v| v.trim().parse().unwrap_or(0));
    while data.len() < head_end + length {
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        data.extend_from_slice(&buf[..n]);
    }
    let mut first = head.lines().next()?.split(' ');
    Some(Request {
        method: first.next()?.to_string(),
        path: first.next()?.to_string(),
        headers: lower,
        body: data[head_end..].to_vec(),
    })
}

fn reply_status(status: u16, extra_headers: &str, body: &[u8]) -> Vec<u8> {
    let mut reply = format!(
        "HTTP/1.1 {} X\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        extra_headers,
        body.len()
    )
    .into_bytes();
    reply.extend_from_slice(body);
    reply
}

fn header<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    headers
        .lines()
        .find_map(|l| l.strip_prefix(name))
        .map(|v| v.trim_start_matches(':').trim())
}

impl Portal {
    fn handle(state: &Mutex<State>, base: &str, stream: &mut TcpStream) {
        let Some(request) = read_request(stream) else {
            return;
        };
        let mut state = state.lock().unwrap();
        state
            .requests
            .push((request.method.clone(), request.path.clone()));

        let upload_id = request
            .path
            .strip_prefix("/upload/")
            .and_then(|id| id.parse::<usize>().ok());
        let reply = match (request.method.as_str(), upload_id) {
            ("POST", None) if request.path.contains("/upload/tus") => {
                match state.create_faults.pop_front() {
                    Some(UploadFault::Status(code)) => reply_status(code, "", b""),
                    Some(_) => return,
                    None => {
                        let length = header(&request.headers, "upload-length")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(0);
                        state.uploads.push(Upload {
                            length,
                            data: Vec::new(),
                        });
                        let id = state.uploads.len() - 1;
                        reply_status(201, &format!("Location: {}/upload/{}\r\n", base, id), b"")
                    }
                }
            }
            ("HEAD", Some(id)) => {
                let upload = &state.uploads[id];
                reply_status(
                    200,
                    &format!(
                        "Upload-Offset: {}\r\nUpload-Length: {}\r\n",
                        upload.data.len(),
                        upload.length
                    ),
                    b"",
                )
            }
            ("PATCH", Some(id)) => {
                let offset: usize = header(&request.headers, "upload-offset")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(usize::MAX);
                match state.patch_faults.pop_front() {
                    Some(UploadFault::Status(code)) => reply_status(code, "", b""),
                    Some(UploadFault::HangUp) => return,
                    fault => {
                        let upload = &mut state.uploads[id];
                        if offset != upload.data.len() {
                            reply_status(409, "", b"")
                        } else {
                            let keep = match fault {
                                Some(UploadFault::KeepOnly(n)) => n.min(request.body.len()),
                                _ => request.body.len(),
                            };
                            upload.data.extend_from_slice(&request.body[..keep]);
                            let at = upload.data.len();
                            reply_status(204, &format!("Upload-Offset: {}\r\n", at), b"")
                        }
                    }
                }
            }
            _ => {
                let fault = state
                    .faults
                    .get_mut(&request.path)
                    .and_then(|queue| queue.pop_front());
                match fault {
                    Some(Fault::Status(code)) => reply_status(code, "", b"error page"),
                    Some(Fault::CutOff) => {
                        b"HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\nConnection: close\r\n\r\npartial"
                            .to_vec()
                    }
                    None => match state.blobs.get(&request.path) {
                        Some(body) => reply_status(200, "", body),
                        None => reply_status(404, "", b"not found"),
                    },
                }
            }
        };
        drop(state);
        let _ = stream.write_all(&reply);
    }

    /// Serve `body` at `path` from now on.
    pub fn serve(&self, path: &str, body: &[u8]) {
        self.state
            .lock()
            .unwrap()
            .blobs
            .insert(path.to_string(), body.to_vec());
    }

    /// Have the next request for `path` go wrong.
    pub fn fail_next(&self, path: &str, fault: Fault) {
        self.state
            .lock()
            .unwrap()
            .faults
            .entry(path.to_string())
            .or_default()
            .push_back(fault);
    }

    pub fn requests_for(&self, path: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(_, p)| p == path)
            .count()
    }

    pub fn uploads(&self) -> Vec<Upload> {
        self.state.lock().unwrap().uploads.clone()
    }

    pub fn fail_next_creates(&self, faults: Vec<UploadFault>) {
        self.state.lock().unwrap().create_faults.extend(faults);
    }

    pub fn fail_next_patches(&self, faults: Vec<UploadFault>) {
        self.state.lock().unwrap().patch_faults.extend(faults);
    }
}

static PORTAL: Lazy<Portal> = Lazy::new(|| {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let state: Arc<Mutex<State>> = Arc::default();
    let shared = state.clone();
    let base = url.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let state = shared.clone();
            let base = base.clone();
            std::thread::spawn(move || Portal::handle(&state, &base, &mut stream));
        }
    });
    storage();
    std::env::set_var("PORTAL_URL", &url);
    std::env::set_var("PORTAL_ENCRYPT_URL", &url);
    std::env::set_var("IPFS_GATEWAY", &url);
    std::env::set_var("TOKEN", "test-token");
    Portal { url, state }
});

/// The portal every job in the test process talks to.
pub fn portal() -> &'static Portal {
    &PORTAL
}

/// Tests that inject upload faults, or count uploads, can't share the portal with
/// other uploads at the same moment.
pub static UPLOAD_TEST_LOCK: Lazy<tokio::sync::Mutex<()>> =
    Lazy::new(|| tokio::sync::Mutex::new(()));
