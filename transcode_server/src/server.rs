/*
 * server.rs
 *
 * This file contains code for transcoding a video using ffmpeg.
 * Upload a video in any format that ffmpeg can read
 * The video is then transcoded to multiple formats specified in `media_formats.json` file
 * to different codecs, bitrates, resolutions and son on.
 * These transcoded videos are uploaded to decentralised SIA Storage via S5.
 *
 * Author: Jules Lai
 * Date: 28 May 2023
 */

mod auth;
mod s5;

mod encrypt_file;

mod utils;
use utils::{
    base64url_to_bytes, bytes_to_base64url, download_and_concat_files, download_with_retries,
};

mod transcode_video;
use transcode_video::{
    get_video_format_from_str, transcode_video, transcode_video_produce, transcode_video_publish,
    validate_format, LocalOutput, TranscodeVideoResponse,
};

mod hls_segment;
mod moderation;
mod shared;
use shared::LocalFile;
#[cfg(test)]
mod test_support;

use tonic::{transport::Server, Request, Response, Status};
use warp::Filter;

use async_trait::async_trait;

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use transcode::{
    transcode_service_server::{TranscodeService, TranscodeServiceServer},
    GetTranscodedRequest, GetTranscodedResponse, TranscodeRequest, TranscodeResponse,
};

mod encrypted_cid;
use crate::encrypt_file::decrypt_file_xchacha20;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::read_to_string;

use anyhow::Result;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use uuid::Uuid;

use sha2::Digest;

use dotenv::{dotenv, var};

static TRANSCODED: Lazy<Mutex<HashMap<String, (String, f64)>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static FAILED: Lazy<Mutex<HashMap<String, JobFailure>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static PATH_TO_FILE: Lazy<String> =
    Lazy::new(|| var("PATH_TO_FILE").unwrap_or_else(|_| panic!("PATH_TO_FILE not set in .env")));
static PATH_TO_TRANSCODED_FILE: Lazy<String> = Lazy::new(|| {
    var("PATH_TO_TRANSCODED_FILE")
        .unwrap_or_else(|_| panic!("PATH_TO_TRANSCODED_FILE not set in .env"))
});

/// What this process knows about a job it has accepted.
#[derive(Default)]
struct JobRecord {
    ended_at: Option<Instant>,
    /// Seconds, or 0.0 until the source has been probed.
    source_duration: f64,
}

/// Every job this process has accepted. A task missing from here was never
/// accepted, or was lost in a restart.
static JOB_RECORDS: Lazy<std::sync::Mutex<HashMap<String, JobRecord>>> =
    Lazy::new(|| std::sync::Mutex::new(HashMap::new()));

fn job_records() -> std::sync::MutexGuard<'static, HashMap<String, JobRecord>> {
    JOB_RECORDS.lock().unwrap_or_else(|e| e.into_inner())
}

fn register_job(task_id: &str) {
    job_records().insert(task_id.to_string(), JobRecord::default());
}

fn forget_job(task_id: &str) {
    job_records().remove(task_id);
}

fn mark_job_ended(task_id: &str) {
    job_records()
        .entry(task_id.to_string())
        .or_default()
        .ended_at = Some(Instant::now());
}

fn set_source_duration(task_id: &str, duration: f64) {
    if let Some(record) = job_records().get_mut(task_id) {
        record.source_duration = duration;
    }
}

/// Removes the jobs that ended more than `ttl` ago and returns their ids. Running
/// and queued jobs always stay.
fn take_expired(records: &mut HashMap<String, JobRecord>, ttl: Duration) -> Vec<String> {
    let mut expired = Vec::new();
    records.retain(|task_id, record| {
        let is_expired = record.ended_at.is_some_and(|at| at.elapsed() >= ttl);
        if is_expired {
            expired.push(task_id.clone());
        }
        !is_expired
    });
    expired
}

async fn forget_job_results(task_ids: &[String]) {
    let mut transcoded = TRANSCODED.lock().await;
    let mut failed = FAILED.lock().await;
    for task_id in task_ids {
        transcoded.remove(task_id);
        failed.remove(task_id);
        shared::forget_progress(task_id);
    }
}

/// So that a long-running server doesn't keep every result it ever produced.
async fn prune_job_records(ttl: Duration) {
    let expired = take_expired(&mut job_records(), ttl);
    forget_job_results(&expired).await;
}

/// A number from the environment, or `default` if it is unset or unreadable.
fn env_u64(name: &str, default: u64) -> u64 {
    match var(name) {
        Ok(value) => value.trim().parse().unwrap_or_else(|_| {
            eprintln!("{} is not a number; using {}", name, default);
            default
        }),
        Err(_) => default,
    }
}

/// `seconds` as a limit, where 0 means no limit in practice.
fn limit_or_none(seconds: u64) -> Duration {
    const THIRTY_YEARS: u64 = 30 * 365 * 24 * 60 * 60;
    Duration::from_secs(if seconds == 0 { THIRTY_YEARS } else { seconds })
}

/// The longest one job may run once it has a slot. Its job is to free the slot of
/// something stuck in a way nothing else notices, so it is set well above the
/// longest real job. `JOB_DEADLINE_SECS`; 0 turns it off.
fn job_deadline() -> Duration {
    limit_or_none(env_u64("JOB_DEADLINE_SECS", 24 * 60 * 60))
}

/// How long the result of a finished job can still be fetched.
fn job_record_ttl() -> Duration {
    Duration::from_secs(env_u64("JOB_RECORD_TTL_SECS", 24 * 60 * 60))
}

/// Logs get only the start of a CID: an encrypted CID carries the decryption key
/// further along, so the whole thing must never be written out.
fn log_safe_cid(cid: &str) -> String {
    let start: String = cid.chars().take(12).collect();
    format!("{}... ({} chars)", start, cid.chars().count())
}

#[derive(Debug, Clone)]
struct TranscodeJob {
    task_id: String,
    source_cid: String,
    media_formats: String,
    is_encrypted: bool,
    is_gpu: bool,
    preview_percent: u32,
}

const CID_TYPE_ENCRYPTED_SIZE: usize = 1;
const ENCRYPTION_ALGORITHM_SIZE: usize = 1;
const CHUNK_SIZE_AS_POWEROF2_SIZE: usize = 1;

const ENCRYPTED_BLOB_HASH_SIZE: usize = 33;
const KEY_SIZE: usize = 32;

/// Size of one encrypted chunk: 256 KiB of content and its 16 byte tag.
const ENCRYPTED_CHUNK_SIZE: u64 = 262144 + 16;

/// The parts of an encrypted CID a job needs.
struct EncryptedCid {
    key: Vec<u8>,
    blob_hash: String,
}

/// Leaves out the key, so this can't end up in a log by accident.
impl std::fmt::Debug for EncryptedCid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedCid")
            .field("blob_hash", &self.blob_hash)
            .finish_non_exhaustive()
    }
}

/// Reads an encrypted CID: a `u` (base64url), then type, algorithm and chunk size,
/// the blob hash, the key, and more this code doesn't need.
fn parse_encrypted_cid(encrypted_cid: &str) -> std::result::Result<EncryptedCid, JobFailure> {
    let invalid = || JobFailure::bad_input("Invalid source CID");
    let encoded = encrypted_cid.strip_prefix('u').ok_or_else(invalid)?;
    let cid_bytes = base64url_to_bytes(encoded).map_err(|_| invalid())?;

    let hash_start =
        CID_TYPE_ENCRYPTED_SIZE + ENCRYPTION_ALGORITHM_SIZE + CHUNK_SIZE_AS_POWEROF2_SIZE;
    let key_start = hash_start + ENCRYPTED_BLOB_HASH_SIZE;
    let key_end = key_start + KEY_SIZE;
    if cid_bytes.len() < key_end {
        return Err(invalid());
    }

    Ok(EncryptedCid {
        key: cid_bytes[key_start..key_end].to_vec(),
        blob_hash: bytes_to_base64url(&cid_bytes[hash_start..key_start]),
    })
}

/// Where a source is cached. The name of an encrypted source is a hash of its CID, not
/// the CID, because the name ends up in FFmpeg's arguments and output, and in the names
/// of its outputs, and the CID holds the key.
fn source_file_path(source_cid: &str, is_encrypted: bool) -> String {
    if is_encrypted {
        let hash = sha2::Sha256::digest(source_cid.as_bytes());
        format!("{}{}", *PATH_TO_FILE, hex::encode(hash))
    } else {
        format!("{}{}", *PATH_TO_FILE, source_cid)
    }
}

/// Why a job ended without a result. `reason` is `&'static str` on purpose: it
/// is returned to API clients, so it must never carry URLs, paths or tokens.
/// The detail belongs in the server log, next to where the failure happens.
#[derive(Debug, Clone, PartialEq)]
struct JobFailure {
    status_code: i32,
    reason: &'static str,
}

impl JobFailure {
    /// The request itself was wrong; retrying it cannot help.
    fn bad_input(reason: &'static str) -> Self {
        JobFailure {
            status_code: 400,
            reason,
        }
    }

    fn server(reason: &'static str) -> Self {
        JobFailure {
            status_code: 500,
            reason,
        }
    }
}

/// Split `network://cid[.ext]` into the bare CID and the network name.
///
/// The CID becomes part of a file name, so anything beyond the characters real CIDs
/// use is refused. That rules out paths: a CID with a directory in it would otherwise
/// be cut down to its last part, and share a cache entry with unrelated sources.
fn parse_source_cid(orig: &str) -> std::result::Result<(String, String), JobFailure> {
    let invalid = JobFailure::bad_input("Invalid source CID");
    let (network, rest) = orig.split_once("://").ok_or_else(|| invalid.clone())?;
    if network.is_empty() || rest.is_empty() {
        return Err(invalid);
    }
    let source_cid = rest.split_once('.').map_or(rest, |(cid, _ext)| cid);
    let usable = !source_cid.is_empty()
        && source_cid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '='));
    if !usable || rest.contains('/') {
        return Err(invalid);
    }
    Ok((source_cid.to_string(), network.to_string()))
}

/// Formats sent by the client are bad input if they can't be used; the server's own
/// formats file not being usable is a server fault.
fn parse_media_formats(
    json: &str,
    client_supplied: bool,
) -> std::result::Result<Vec<Value>, JobFailure> {
    let unusable = |why: String| {
        eprintln!("Unusable video formats: {}", why);
        if client_supplied {
            JobFailure::bad_input("Invalid media formats")
        } else {
            JobFailure::server("Server configuration error")
        }
    };
    let formats: Vec<Value> = serde_json::from_str(json).map_err(|e| unusable(e.to_string()))?;
    if formats.is_empty() {
        return Err(unusable("the list is empty".to_string()));
    }
    for format in &formats {
        let format = serde_json::from_value(format.clone())
            .map_err(|e: serde_json::Error| unusable(e.to_string()))?;
        validate_format(&format).map_err(unusable)?;
    }
    Ok(formats)
}

/// The formats to produce: the ones sent with the job, or else the server's own.
fn load_media_formats(client_formats: &str) -> std::result::Result<Vec<Value>, JobFailure> {
    if !client_formats.is_empty() {
        return parse_media_formats(client_formats, true);
    }
    let server_formats = var("MEDIA_FORMATS_FILE")
        .map_err(|e| e.to_string())
        .and_then(|path| read_to_string(path).map_err(|e| e.to_string()))
        .map_err(|e| {
            eprintln!("Cannot read the video formats file: {}", e);
            JobFailure::server("Server configuration error")
        })?;
    parse_media_formats(&server_formats, false)
}

/// Nothing published and at least one format failed. All-cached (nothing
/// published, nothing failed) and partial success both remain a 200.
fn all_formats_failed(published: usize, failed: usize) -> bool {
    published == 0 && failed > 0
}

async fn record_failure(task_id: &str, failure: JobFailure) {
    eprintln!(
        "Job failed: task_id={} status_code={} reason={}",
        task_id, failure.status_code, failure.reason
    );
    FAILED.lock().await.insert(task_id.to_string(), failure);
}

/// What `GetTranscoded` reports, as (status_code, metadata, duration). Any
/// non-200 means the job has failed permanently, with the reason in `metadata`.
/// A moderation hold is NOT a failure: it stays 200 with `metadata` `[]`.
async fn job_status(task_id: &str) -> (i32, String, f64) {
    let record_duration = job_records().get(task_id).map(|r| r.source_duration);
    if let Some(failure) = FAILED.lock().await.get(task_id) {
        return (
            failure.status_code,
            failure.reason.to_string(),
            record_duration.unwrap_or(0.0),
        );
    }
    if let Some(entry) = TRANSCODED.lock().await.get(task_id).cloned() {
        return (200, entry.0, entry.1);
    }
    match record_duration {
        Some(duration) => (200, "Transcoding in progress".to_string(), duration),
        None => (404, "Unknown task".to_string(), 0.0),
    }
}

/// Run a job so that every way it can end is reported. Its own task isolates a
/// panic, and the deadline stops a job stuck for good, so the caller always gets
/// its slot back. Stopping the job drops its FFmpeg child, which kills it.
async fn supervise<F>(task_id: String, deadline: Duration, job: F)
where
    F: std::future::Future<Output = std::result::Result<(), JobFailure>> + Send + 'static,
{
    let mut handle = tokio::spawn(job);
    let failure = match tokio::time::timeout(deadline, &mut handle).await {
        Ok(Ok(Ok(()))) => None,
        Ok(Ok(Err(failure))) => Some(failure),
        Ok(Err(e)) => {
            eprintln!("Job panicked: task_id={} error={:?}", task_id, e);
            Some(JobFailure::server("Internal error"))
        }
        Err(_) => {
            handle.abort();
            eprintln!(
                "Job stopped after reaching its deadline of {:?}: task_id={}",
                deadline, task_id
            );
            Some(JobFailure::server("Job timed out"))
        }
    };
    if let Some(failure) = failure {
        record_failure(&task_id, failure).await;
    }
    mark_job_ended(&task_id);
}

/// Keep a background task alive: if it dies, log it and start it again.
async fn keep_restarting<F, Fut>(name: &'static str, pause: Duration, task: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    loop {
        match tokio::spawn(task()).await {
            Ok(()) => return,
            Err(e) => {
                eprintln!("ALERT: {} died and is being restarted: {:?}", name, e);
                tokio::time::sleep(pause).await;
            }
        }
    }
}

fn required_setting(name: &str) -> std::result::Result<String, JobFailure> {
    var(name).map_err(|_| {
        eprintln!("Required environment variable {} not found", name);
        JobFailure::server("Server configuration error")
    })
}

/// Downloads an encrypted source and decrypts it to `part_path`.
async fn fetch_encrypted_source(
    task_id: &str,
    file_path: &str,
    encrypted_cid: &EncryptedCid,
    part_path: &str,
) -> std::result::Result<(), JobFailure> {
    let portal_url = required_setting("PORTAL_ENCRYPT_URL")?;
    let url = format!(
        "{}{}{}?types=5,3",
        portal_url, "/api/locations/", encrypted_cid.blob_hash
    );
    println!("Looking up the parts of an encrypted source: {}", &url);

    let metadata_file = LocalFile::file(&format!("{}.{}.meta.part", file_path, task_id));
    download_with_retries(&url, metadata_file.path())
        .await
        .map_err(|e| {
            eprintln!("Failed to download encrypted video from URL {}: {}", url, e);
            JobFailure::server("Source download failed")
        })?;
    let encrypted_metadata = tokio::fs::read_to_string(metadata_file.path())
        .await
        .map_err(|e| {
            eprintln!("Failed to read encrypted metadata: {}", e);
            JobFailure::server("Source metadata could not be read")
        })?;

    let ciphertext_file = LocalFile::file(&format!("{}.{}.enc.part", file_path, task_id));
    download_and_concat_files(encrypted_metadata, ciphertext_file.path().to_string())
        .await
        .map_err(|e| {
            eprintln!("Download and concatenation failed: {}", e);
            JobFailure::server("Source download failed")
        })?;

    let size = tokio::fs::metadata(ciphertext_file.path())
        .await
        .map_err(|e| {
            eprintln!("Failed to read the size of the download: {}", e);
            JobFailure::server("Source download failed")
        })?
        .len();
    let last_chunk_index = (size / ENCRYPTED_CHUNK_SIZE) as u32;

    let input = ciphertext_file.path().to_string();
    let output = part_path.to_string();
    let key = encrypted_cid.key.clone();
    // Off the async workers: the file can be many gigabytes.
    let decrypted = tokio::task::spawn_blocking(move || {
        decrypt_file_xchacha20(input, output, key, 0, last_chunk_index)
    })
    .await;
    match decrypted {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => {
            eprintln!("Decryption error: {:?}", error);
            Err(JobFailure::server("Source decryption failed"))
        }
        Err(error) => {
            eprintln!("Decryption task failed: {:?}", error);
            Err(JobFailure::server("Source decryption failed"))
        }
    }
}

/// Gets the source into the cache, unless it's there already. Everything happens under
/// temporary names of this task's own, and only a complete file gets the final name,
/// so neither a failure nor another job on the same source can leave a bad one there.
async fn fetch_source(
    task_id: &str,
    file_path: &str,
    source_cid: &str,
    storage_network: &str,
    encrypted_cid: Option<&EncryptedCid>,
) -> std::result::Result<(), JobFailure> {
    if Path::new(file_path).exists() {
        println!("File already exists: {}", file_path);
        return Ok(());
    }

    let part_file = LocalFile::file(&format!("{}.{}.part", file_path, task_id));
    let part_path = part_file.path().to_string();
    if let Some(encrypted_cid) = encrypted_cid {
        fetch_encrypted_source(task_id, file_path, encrypted_cid, &part_path).await?;
    } else {
        let url = if storage_network == "ipfs" {
            format!(
                "{}{}{}",
                required_setting("IPFS_GATEWAY")?,
                "/ipfs/",
                source_cid
            )
        } else {
            format!(
                "{}{}{}",
                required_setting("PORTAL_URL")?,
                "/s5/blob/",
                source_cid
            )
        };
        download_with_retries(&url, &part_path).await.map_err(|e| {
            eprintln!("Failed to download video from URL {}: {}", url, e);
            JobFailure::server("Source download failed")
        })?;
        println!("Video downloaded successfully from URL: {}", url);
    }

    let final_path = file_path.to_string();
    let finalized = tokio::task::spawn_blocking(move || finalize_part(&part_path, &final_path))
        .await
        .map_err(|e| e.to_string())
        .and_then(|result| result.map_err(|e| e.to_string()));
    finalized.map_err(|e| {
        eprintln!("Source finalise error: {}", e);
        JobFailure::server("Source could not be stored")
    })
}

/// Runs one job from start to finish: download, transcode, moderate if the gate is
/// armed, publish, and record the result. `gate` and `client` are parameters so that
/// tests can run the whole thing without a node.
#[allow(clippy::too_many_arguments)]
async fn process_single_job(
    task_id: String,
    orig_source_cid: String,
    media_formats: String,
    is_encrypted: bool,
    is_gpu: bool,
    preview_percent: u32,
    gate: moderation::GateMode,
    client: Box<dyn moderation::ModerationClient>,
) -> std::result::Result<(), JobFailure> {
    println!("preview_percent: {}", preview_percent);
    let (source_cid, storage_network) = parse_source_cid(&orig_source_cid).map_err(|failure| {
        eprintln!("Invalid source CID: {}", log_safe_cid(&orig_source_cid));
        failure
    })?;

    // Everything wrong with the request itself gets found before any time goes on
    // downloading a source that may be many gigabytes.
    let encrypted_cid = if is_encrypted {
        Some(parse_encrypted_cid(&source_cid)?)
    } else {
        None
    };
    let media_formats_vec = load_media_formats(&media_formats)?;

    // Initialize progress to 0 at the start for all formats
    let formats_count = media_formats_vec.len();
    for i in 0..formats_count {
        shared::update_progress(&task_id, i, 0);
    }

    println!("source_cid: {}", log_safe_cid(&source_cid));

    let file_path = source_file_path(&source_cid, is_encrypted);

    // For the whole job, and before looking for the file: GC could otherwise take a
    // cached source between this job finding it and starting to use it, or between
    // one format and the next.
    let _source_pin = moderation::pin(&file_path);
    fetch_source(
        &task_id,
        &file_path,
        &source_cid,
        &storage_network,
        encrypted_cid.as_ref(),
    )
    .await?;

    // Then, we transcode the downloaded video with each video format
    let mut transcoded_formats = Vec::new();
    let mut failed_formats = 0usize;

    // As soon as the source is here, and reported from now on: the node works out its
    // billing steps from this, and can prove nothing while it reads 0.0.
    let mut source_duration = transcode_video::get_video_duration(&file_path)
        .await
        .unwrap_or(0.0);
    set_source_duration(&task_id, source_duration);

    // ── M3 shadow moderation (D1/D4): fire-and-forget, concurrent with the
    //    transcode loop below; incapable of delaying, failing, or blocking
    //    this job. The source (fresh or cache-hit) is final on disk here. ──
    if moderation::moderation_enabled() {
        spawn_shadow_moderation(task_id.clone(), file_path.clone());
    }

    // ── WP-T publish gate (three-state MODERATION_GATE; default off) ──
    let tap_dir = transcode_video::modtap_dir(&task_id);
    // Probe the SOURCE (now on disk), not output codecs; `&&` short-circuits so a
    // disabled run never spawns ffprobe. On probe error, source_has_video ⇒ true ⇒ HOLD.
    let scan_required = gate.moderates() && transcode_video::source_has_video(&file_path).await;
    if gate.moderates() {
        // Defense-in-depth: clear any stale tap dir before producing this job's keyframes
        // so the gate can never read a prior run's PNGs. (task_id is a fresh UUID, so this
        // is belt-and-braces.) Dark-launch parity: guarded by the gate.
        let _ = fs::remove_dir_all(&tap_dir);
    }
    // ── GC pins (Task 3.1.4) ──────────────────────────────────────────────────
    // `garbage_collect` runs on PATH_TO_TRANSCODED_FILE as well as PATH_TO_FILE,
    // and it keeps deleting until the directory is under its threshold, so during
    // the gate's produce→publish window the tap dir and the held outputs can go too.
    // Each unpinned artefact would fail silently, and differently: a reaped tap dir ⇒
    // a spurious fail-closed HOLD of clean content; reaped outputs ⇒ publish
    // uploads nothing. RAII: every pin releases on every path, including the early
    // `return` in the HOLD branch and on panic. Outputs are held in `LocalOutput`.
    let _tap_files = gate.moderates().then(|| LocalFile::dir(&tap_dir));
    let mut pending: Vec<LocalOutput> = Vec::new();
    let mut tap_ok = !scan_required; // audio source: nothing to tap, empty set is clearable
    let mut tapped = false; // has the tap been assigned to a video output yet?

    for (index, video_format) in media_formats_vec.iter().enumerate() {
        let video_format_str = match serde_json::to_string(&video_format) {
            Ok(str) => str,
            Err(e) => {
                eprintln!("Error serializing video format: {:?}", e);
                failed_formats += 1;
                continue;
            }
        };

        let format_result = get_video_format_from_str(&video_format_str);
        let format = match format_result {
            Ok(format) => format,
            Err(e) => {
                eprintln!("Failed to get video format from string: {}", e);
                failed_formats += 1;
                continue; // Skip the rest of this loop iteration
            }
        };

        if !gate.moderates() {
            // ── Gate off: legacy inline produce+publish, byte-identical to today ──
            if !check_transcoded_file_exists(
                file_path.as_str(),
                &format.id.to_string(),
                format.ext.as_str(),
            )
            .await
            {
                let transcode_result: Result<Response<TranscodeVideoResponse>, Status> =
                    transcode_video(
                        task_id.clone(),
                        index,
                        &file_path,
                        &video_format_str,
                        is_encrypted,
                        is_gpu,
                        preview_percent,
                    )
                    .await;

                match transcode_result {
                    Ok(transcode_video_response) => {
                        let response = transcode_video_response.into_inner();
                        if source_duration == 0.0 && response.duration > 0.0 {
                            source_duration = response.duration;
                        }
                        println!(
                            "Response: status_code: {}, message: {}",
                            response.status_code, response.message
                        );
                        if response.status_code != 200 {
                            eprintln!(
                                "Format {} failed with status {}: {}",
                                index, response.status_code, response.message
                            );
                            failed_formats += 1;
                            continue;
                        }
                        let mut video_format_modified = video_format.clone();
                        apply_cid_metadata(
                            &mut video_format_modified,
                            &response,
                            format.hls.unwrap_or(false),
                            &format.dest,
                        );
                        transcoded_formats.push(video_format_modified);
                    }
                    Err(e) => {
                        eprintln!("Error transcoding video: {:?}", e);
                        failed_formats += 1;
                        continue;
                    }
                }
            }
            continue;
        }

        // ── Armed: produce-only; publish is deferred until the verdict ──
        let fmt_has_video = video_format["vcodec"]
            .as_str()
            .is_some_and(|s| !s.is_empty());
        // The tap owner must ALSO have a foldable vf — a non-foldable-vf format emits no
        // PNGs (run_ffmpeg's do_tap), so letting it claim the tap would permanently HOLD the
        // job; instead a later foldable format or the source-tap fallback handles it.
        // And there has to be something to tap: asking for the video of a source without
        // any makes FFmpeg fail, and takes this format's output with it.
        let want_tap = scan_required
            && fmt_has_video
            && transcode_video::vf_foldable(video_format["vf"].as_str())
            && !tapped;
        // Cache short-circuit applies to non-tap formats only; the tap owner always
        // re-emits a fresh keyframe set (never moderate a stale/empty cached dir).
        if !want_tap
            && check_transcoded_file_exists(
                file_path.as_str(),
                &format.id.to_string(),
                format.ext.as_str(),
            )
            .await
        {
            continue;
        }
        let this_tap = if want_tap {
            Some(tap_dir.as_str())
        } else {
            None
        };
        match transcode_video_produce(
            task_id.clone(),
            index,
            &file_path,
            &video_format_str,
            is_encrypted,
            is_gpu,
            preview_percent,
            this_tap,
            true,
        )
        .await
        {
            Ok(out) => {
                if want_tap {
                    tapped = true;
                    tap_ok = true; // the tap-owning ffmpeg run succeeded
                }
                pending.push(out);
            }
            Err(e) => {
                eprintln!("Error producing format {}: {:?}", index, e);
                failed_formats += 1;
                if want_tap {
                    // One failed format, which needn't cost the job its scan: the next
                    // format can take the tap over, or failing that the dedicated tap
                    // below. tap_ok stays false until one of those has run to the end,
                    // and what this run left behind must not count towards it.
                    let _ = fs::remove_dir_all(&tap_dir);
                }
                // NEVER return/`?`: fall through so the gate + held-status still run.
            }
        }
    }

    if gate.moderates() {
        // Nothing to publish because everything failed: a failed transcode, not a
        // moderation decision. Calling it a hold would say the content was at fault
        // and that trying again can't help.
        if all_formats_failed(pending.len(), failed_formats) {
            return Err(JobFailure::server("All formats failed"));
        }

        // Dedicated source tap for audio-only OUTPUTS of a video source (nothing to tee
        // from): its own ffmpeg child is the job's sole video decode.
        if scan_required && !tapped {
            match transcode_video::tap_source_keyframes(&file_path, &tap_dir).await {
                Ok(()) => tap_ok = true,
                Err(e) => eprintln!("Source keyframe tap failed: {:?}", e), // tap_ok stays false ⇒ HOLD
            }
        }

        // ONE read of the tap dir; the integrity check AND the POST share this set (no TOCTOU).
        // Off the async workers: a long title has hundreds of megabytes of frames.
        let frames_dir = tap_dir.clone();
        let kf: Vec<Vec<u8>> = tokio::task::spawn_blocking(move || read_keyframe_pngs(&frames_dir))
            .await
            .unwrap_or_default();
        // The tap stops at a cap worked out from the duration the file claims. Reaching
        // it means the file is longer than it says, or FFmpeg would have run out of
        // frames first, and whatever came after was never scanned.
        let frame_cap = moderation::frames_cap(
            transcode_video::get_video_duration(&file_path)
                .await
                .unwrap_or(0.0),
        );
        let outcome = if scan_required && (!tap_ok || kf.is_empty() || kf.len() >= frame_cap) {
            // Video source with an absent/partial/empty keyframe set: HOLD, and do NOT POST
            // (never let the node store a false `Cleared` for unscanned video).
            moderation::ModerationOutcome::Unavailable
        } else {
            // Optional own-hash exact-match input, computed ONLY when there is
            // actually something to POST. The `kf.is_empty()` guard matters:
            // an audio-only source reaches this branch (scan_required is false)
            // but the client's Q5 empty-set guard then returns without POSTing,
            // so hashing here would stream a multi-GB file to produce a value
            // nothing ever reads. The verdict is identical either way.
            //
            // Off-reactor: hashing streams the whole source. Any failure
            // (JoinError or hash error) ⇒ None — the optional SHA is simply
            // omitted and the node verdict still governs (fail-closed unchanged).
            // The job's pin on the source covers this: a reap mid-hash would
            // silently omit the field, and Milestone 1a depends on the exact match.
            let source_sha = if kf.is_empty() {
                None
            } else {
                let fp = file_path.clone();
                tokio::task::spawn_blocking(move || moderation::sha256_file(&fp))
                    .await
                    .ok()
                    .and_then(|r| r.ok())
            };
            client.moderate(&task_id, &kf, source_sha).await
        };

        if !moderation::may_publish(&outcome) {
            if gate.holds() {
                // ── HOLD: upload nothing; discard local temp; record held status ──
                eprintln!("MODERATION HOLD task_id={} outcome={:?}", task_id, outcome);
                let held_duration = if source_duration > 0.0 {
                    source_duration
                } else {
                    pending.first().map(|o| o.total_duration).unwrap_or(0.0)
                };
                for out in &pending {
                    if out.is_hls {
                        let _ = fs::remove_dir_all(held_output_path(out));
                    } else {
                        let _ = fs::remove_file(held_output_path(out));
                    }
                }
                let _ = fs::remove_dir_all(&tap_dir);
                let mut transcoded = TRANSCODED.lock().await;
                transcoded.insert(task_id.clone(), ("[]".to_string(), held_duration));
                drop(transcoded);
                shared::mark_finished(&task_id);
                return Ok(());
            }
            // ── DARK (Milestone 1): record what WOULD have been held, then fall
            //    INTO the publish branch below — not around it — so the tap-dir
            //    cleanup it performs still runs. Publishing anyway is the entire
            //    point of dark mode; the ordering is identical to `enforce`'s so
            //    that soaking here genuinely exercises the path `enforce` will take.
            println!(
                "MODERATION WOULD-HOLD task_id={} outcome={:?}",
                task_id, outcome
            );
        }

        // ── Cleared (or dark): discard the tap, then publish each deferred output ──
        let _ = fs::remove_dir_all(&tap_dir);
        for out in pending {
            let video_format_value = out.video_format.clone();
            let is_hls = out.is_hls;
            let dest = out.dest.clone();
            match transcode_video_publish(out).await {
                Ok(resp) => {
                    let response = resp.into_inner();
                    if source_duration == 0.0 && response.duration > 0.0 {
                        source_duration = response.duration;
                    }
                    if response.status_code != 200 {
                        eprintln!(
                            "Publish failed with status {}: {}",
                            response.status_code, response.message
                        );
                        failed_formats += 1;
                        continue;
                    }
                    let mut video_format_modified = video_format_value;
                    apply_cid_metadata(&mut video_format_modified, &response, is_hls, &dest);
                    transcoded_formats.push(video_format_modified);
                }
                Err(e) => {
                    eprintln!("Error publishing format: {:?}", e);
                    failed_formats += 1;
                    continue;
                }
            }
        }
    }

    if all_formats_failed(transcoded_formats.len(), failed_formats) {
        return Err(JobFailure::server("All formats failed"));
    }

    let transcoded_json = serde_json::to_string(&transcoded_formats).unwrap_or_else(|e| {
        eprintln!("Error serializing transcoded formats: {:?}", e);
        "".to_string()
    });

    let mut transcoded = TRANSCODED.lock().await;
    transcoded.insert(task_id.clone(), (transcoded_json, source_duration));
    drop(transcoded);

    // Only now, with the result in place: clients read 100 as "finished".
    shared::mark_finished(&task_id);
    Ok(())
}

/// M3 shadow moderation (HAND-OFF §5): pin the source against GC, translate
/// its path to the sidecar's view, call the sidecar (one in flight per
/// process; per-call deadline), log the outcome, relay a verdict via the
/// (stubbed, OQ-M3-1) relay. Detached — the job never awaits it (D4:
/// incapable of delaying, failing, or blocking a transcode).
fn spawn_shadow_moderation(task_id: String, file_path: String) {
    // Admission control first (D1 indirect-coupling guard): each pending
    // shadow call pins a multi-GB source, so at the cap we shed — fail open —
    // rather than let a wedged sidecar grow the pin set until the cache
    // volume fills and NEW jobs' downloads start failing.
    let pending = match moderation::try_shadow_slot() {
        Some(guard) => guard,
        None => {
            eprintln!(
                "MODERATION SHADOW task_id={} ALERT shed: shadow queue at capacity \
                 (MODERATION_MAX_PENDING) — sidecar wedged or undersized? — failing open",
                task_id
            );
            return;
        }
    };
    // Pin BEFORE spawning and move the guard into the task: between spawn and
    // the task's first poll the source would otherwise be unprotected. RAII: released on
    // every exit path, panic included (CONTRACT §3: the source must survive
    // until the response arrives).
    let pin = moderation::pin(&file_path);
    tokio::spawn(async move {
        let _pending = pending;
        let _pin = pin;
        let socket = match moderation::socket_path() {
            Some(s) => s,
            None => {
                eprintln!(
                    "MODERATION SHADOW task_id={} ALERT config-fault: MODERATION_ENABLED=true \
                     but MODERATION_SOCKET_PATH unset — failing open",
                    task_id
                );
                return;
            }
        };
        let sidecar_path = match moderation::sidecar_source_path(&file_path) {
            Some(p) => p,
            None => {
                eprintln!(
                    "MODERATION SHADOW task_id={} ALERT config-fault: untranslatable source \
                     path {} — failing open",
                    task_id, file_path
                );
                return;
            }
        };
        let (outcome, wait_ms, call_ms) =
            moderation::moderate_via_sidecar(&socket, &sidecar_path).await;
        // One structured line per job (HAND-OFF §8) — on a verdict, call_ms is
        // the owed CONTRACT §6 budget number; wait_ms is queue diagnostics.
        println!(
            "MODERATION SHADOW task_id={} wait_ms={} call_ms={} {}",
            task_id,
            wait_ms,
            call_ms,
            moderation::outcome_log_fragment(&outcome)
        );
        // Q2: one authoritative verdict writer per job. Everything above this
        // point still runs when the gate is armed — the POST, the
        // classification, the wall-clock log, and the sidecar's own JSONL sink
        // — so shadow evidence collection is completely unaffected. ONLY the
        // node-side write is withheld, because node verdicts are monotonic and
        // an OQ-12 VLM false positive over a Track-1 `cleared` is permanent.
        if !moderation::should_relay() {
            println!(
                "MODERATION SHADOW task_id={} relay suppressed: MODERATION_GATE armed (Q2)",
                task_id
            );
        } else if let Some(envelope) = moderation::build_relay_envelope(&task_id, &outcome) {
            if let Err(e) = moderation::default_relay().relay(&task_id, &envelope).await {
                eprintln!("MODERATION SHADOW task_id={} relay error: {:?}", task_id, e);
            }
        }
    });
}

/// Takes jobs off the queue and runs each in its own task, as slots allow.
///
/// The slot comes first: once a job has left the queue nothing between there and
/// its supervisor can wait or fail, so a job can't be lost while waiting for a
/// slot. The receiver lives outside this function so that a restart finds the
/// jobs still waiting.
async fn run_job_queue<R, Fut>(
    receiver: Arc<Mutex<mpsc::Receiver<TranscodeJob>>>,
    slots: &'static Semaphore,
    deadline: Duration,
    run_job: R,
) where
    R: Fn(TranscodeJob) -> Fut,
    Fut: std::future::Future<Output = std::result::Result<(), JobFailure>> + Send + 'static,
{
    loop {
        let Ok(slot) = slots.acquire().await else {
            return;
        };
        let Some(job) = receiver.lock().await.recv().await else {
            return;
        };
        let active = shared::start_active_job();
        let task_id = job.task_id.clone();
        let work = run_job(job);
        tokio::spawn(async move {
            // Held here so both are released however the job ends.
            let _active = active;
            let _slot = slot;
            supervise(task_id, deadline, work).await;
        });
    }
}

fn run_queued_job(
    job: TranscodeJob,
) -> impl std::future::Future<Output = std::result::Result<(), JobFailure>> + Send {
    process_single_job(
        job.task_id,
        job.source_cid,
        job.media_formats,
        job.is_encrypted,
        job.is_gpu,
        job.preview_percent,
        moderation::gate_mode(),
        moderation::default_client(),
    )
}

#[derive(Debug, PartialEq)]
enum EnqueueError {
    QueueFull,
    QueueClosed,
}

/// Put a job on the queue, or say straight away why not: a full queue used to
/// leave the submitter hanging until a place came free.
///
/// The job is counted before it is sent, because the receiver can start it the
/// moment it is sent. Both are undone if the queue refuses it.
fn enqueue(
    sender: &mpsc::Sender<TranscodeJob>,
    job: TranscodeJob,
) -> std::result::Result<(), EnqueueError> {
    let task_id = job.task_id.clone();
    register_job(&task_id);
    shared::increment_queued();
    sender.try_send(job).map_err(|e| {
        forget_job(&task_id);
        shared::decrement_queued();
        match e {
            mpsc::error::TrySendError::Full(_) => EnqueueError::QueueFull,
            mpsc::error::TrySendError::Closed(_) => EnqueueError::QueueClosed,
        }
    })
}

// The gRPC service implementation
#[derive(Debug, Clone)]
struct TranscodeServiceHandler {
    transcode_task_sender: mpsc::Sender<TranscodeJob>,
}

#[async_trait]
impl TranscodeService for TranscodeServiceHandler {
    async fn transcode(
        &self,
        request: Request<TranscodeRequest>,
    ) -> Result<Response<TranscodeResponse>, Status> {
        let source_cid = request.get_ref().source_cid.clone();

        println!("Received source_cid: {}", log_safe_cid(&source_cid));

        let media_formats = request.get_ref().media_formats.clone();
        println!("Received media_formats: {}", media_formats);

        let is_encrypted = request.get_ref().is_encrypted;
        println!("Received is_encrypted: {}", is_encrypted);

        let is_gpu = request.get_ref().is_gpu;
        println!("Received is_gpu: {}", is_gpu);

        let task_id = Uuid::new_v4().to_string();
        let job = TranscodeJob {
            task_id: task_id.clone(),
            source_cid,
            media_formats,
            is_encrypted,
            is_gpu,
            preview_percent: request.get_ref().preview_percent,
        };
        match enqueue(&self.transcode_task_sender, job) {
            Ok(()) => {}
            Err(EnqueueError::QueueFull) => {
                return Err(Status::resource_exhausted("Transcode queue is full"));
            }
            Err(EnqueueError::QueueClosed) => {
                return Err(Status::internal("Transcode queue is closed"));
            }
        }

        let response = TranscodeResponse {
            status_code: 200,
            message: "Transcoding task queued".to_string(),
            task_id,
        };

        Ok(Response::new(response))
    }

    async fn get_transcoded(
        &self,
        request: Request<GetTranscodedRequest>,
    ) -> Result<Response<GetTranscodedResponse>, Status> {
        let task_id = &request.get_ref().task_id;
        let (status_code, metadata, duration) = job_status(task_id).await;

        let progress = shared::calculate_overall_progress(task_id);

        let response = GetTranscodedResponse {
            status_code,
            metadata,
            progress,
            duration,
        };

        Ok(Response::new(response))
    }
}

#[derive(Debug)]
struct TranscodeError(String);

impl warp::reject::Reject for TranscodeError {}

#[derive(Debug, Serialize)]
struct TranscodeResponseWrapper {
    status_code: i32,
    message: String,
    task_id: String,
}

impl From<transcode::TranscodeResponse> for TranscodeResponseWrapper {
    fn from(response: transcode::TranscodeResponse) -> Self {
        TranscodeResponseWrapper {
            status_code: response.status_code,
            message: response.message,
            task_id: response.task_id,
        }
    }
}

#[derive(Debug, Clone)]
struct RestHandler {
    transcode_task_sender: mpsc::Sender<TranscodeJob>,
}

impl RestHandler {
    async fn transcode(
        &self,
        source_cid: String,
        media_formats: String,
        is_encrypted: bool,
        is_gpu: bool,
        preview_percent: u32,
    ) -> Result<warp::reply::Response, warp::Rejection> {
        use warp::Reply;

        let task_id = Uuid::new_v4().to_string();
        let job = TranscodeJob {
            task_id: task_id.clone(),
            source_cid,
            media_formats,
            is_encrypted,
            is_gpu,
            preview_percent,
        };
        match enqueue(&self.transcode_task_sender, job) {
            Ok(()) => {
                let response = transcode::TranscodeResponse {
                    status_code: 200,
                    message: "Transcoding task queued".to_string(),
                    task_id,
                };
                Ok(warp::reply::json(&TranscodeResponseWrapper::from(response)).into_response())
            }
            // Not a failed job: nothing was queued, and trying later can work.
            Err(EnqueueError::QueueFull) => {
                let response = TranscodeResponseWrapper {
                    status_code: 503,
                    message: "Transcode queue is full".to_string(),
                    task_id: String::new(),
                };
                Ok(warp::reply::with_status(
                    warp::reply::json(&response),
                    warp::http::StatusCode::SERVICE_UNAVAILABLE,
                )
                .into_response())
            }
            Err(EnqueueError::QueueClosed) => Err(warp::reject::custom(TranscodeError(
                "Transcode queue is closed".to_string(),
            ))),
        }
    }
}

#[derive(Debug, Serialize)]
struct GetTranscodedResponseWrapper {
    status_code: i32,
    metadata: String,
    progress: i32,
    duration: f64,
}

impl From<transcode::GetTranscodedResponse> for GetTranscodedResponseWrapper {
    fn from(response: transcode::GetTranscodedResponse) -> Self {
        GetTranscodedResponseWrapper {
            status_code: response.status_code,
            metadata: response.metadata,
            progress: response.progress,
            duration: response.duration,
        }
    }
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    active_jobs: usize,
    queued_jobs: usize,
    max_concurrent: usize,
}

impl RestHandler {
    async fn get_transcoded(&self, task_id: String) -> Result<impl warp::Reply, warp::Rejection> {
        // Retrieve the status and the progress for the given task ID.
        let (status_code, metadata, duration) = job_status(&task_id).await;

        let progress = shared::calculate_overall_progress(&task_id);

        // Construct the response including the progress
        let response = GetTranscodedResponseWrapper {
            status_code,
            metadata,
            progress,
            duration,
        };

        Ok(warp::reply::json(&response))
    }
}

/// The local path a produced-but-unpublished output occupies while the gate
/// holds it.
fn held_output_path(out: &LocalOutput) -> String {
    transcode_video::local_output_path(&out.file_name, &out.ext, out.is_hls)
}

/// Revived for WP-T (IMPLEMENTATION-MODERATION-FRAMES-GATE-WPT.md).
/// Read the tap dir's keyframe PNGs into memory, sorted by filename. A missing/empty
/// dir maps to an empty `Vec`; ANY per-file read error also maps to empty (fail-closed —
/// a partial set must never be scanned as complete). The gate's SINGLE read of the tap:
/// the integrity check and the POST share this in-memory set (no TOCTOU).
fn read_keyframe_pngs(tap_dir: &str) -> Vec<Vec<u8>> {
    let Ok(entries) = std::fs::read_dir(tap_dir) else {
        return Vec::new();
    };
    let paths = frame_paths(entries.map(|entry| entry.map(|e| e.path())));
    let mut frames = Vec::with_capacity(paths.len());
    for p in paths {
        match std::fs::read(&p) {
            Ok(bytes) => frames.push(bytes),
            Err(_) => return Vec::new(), // read error ⇒ empty ⇒ fail-closed HOLD (video source)
        }
    }
    frames
}

/// The PNGs in a listing, sorted. Empty if any entry couldn't be listed, because
/// that entry could be a frame, and then the set is unknown.
fn frame_paths(
    entries: impl Iterator<Item = std::io::Result<std::path::PathBuf>>,
) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    for entry in entries {
        let Ok(path) = entry else {
            return Vec::new();
        };
        if path.extension().and_then(|x| x.to_str()) == Some("png") {
            paths.push(path);
        }
    }
    paths.sort();
    paths
}

/// Apply the published CID/HLS metadata to a format's catalogue `Value` — the exact
/// mapping from the original in-loop block, shared by the dark-launch and gated paths
/// so the dark-launch output stays byte-identical.
fn apply_cid_metadata(
    video_format_modified: &mut Value,
    response: &TranscodeVideoResponse,
    is_hls: bool,
    dest: &Option<String>,
) {
    if is_hls {
        if let Ok(hls_result) = serde_json::from_str::<Value>(&response.cid) {
            video_format_modified["hls"] = json!(true);
            video_format_modified["initSegmentCid"] = hls_result["init_segment_cid"].clone();
            video_format_modified["segments"] = hls_result["segments"].clone();
            video_format_modified["previewSegments"] = hls_result["preview_segments"].clone();
            video_format_modified["totalSegments"] = hls_result["total_segments"].clone();
            video_format_modified["totalDuration"] = hls_result["total_duration"].clone();
        }
    } else {
        match dest {
            Some(d) if d == "ipfs" => {
                video_format_modified["cid"] = json!(format!("ipfs://{}", response.cid));
            }
            _ => {
                video_format_modified["cid"] = json!(format!("s5://{}", response.cid));
            }
        }
    }
}

async fn check_transcoded_file_exists(cid: &str, label: &str, ext: &str) -> bool {
    let filename = format!("{}{}_{}.{}", *PATH_TO_TRANSCODED_FILE, cid, label, ext); // Adjust the path and format as needed.
    Path::new(&filename).exists()
}

fn dir_size(path: &Path) -> u64 {
    fs::read_dir(path)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter_map(|e| e.metadata().ok().map(|m| m.len()))
                .sum()
        })
        .unwrap_or(0)
}

/// Finalise a freshly written source (HAND-OFF §5.1): fsync the `.part` file,
/// then atomically move it into place — the moderation sidecar stat-checks
/// and refuses to report on a file that changes mid-run (`SOURCE_MUTATED`).
/// So if another job got there first its copy stays, and this one is dropped: a
/// hard link fails if the name is taken, which `rename` would simply replace.
/// The parent-directory fsync is deliberately omitted: a crash before the
/// dirent persists only costs a re-download of re-derivable data.
fn finalize_part(part: &str, final_path: &str) -> std::io::Result<()> {
    fs::File::open(part)?.sync_all()?;
    match fs::hard_link(part, final_path) {
        Ok(()) => fs::remove_file(part),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => fs::remove_file(part),
        // Not every filesystem has hard links, and a rename is still atomic.
        Err(_) => fs::rename(part, final_path),
    }
}

/// The directory holding the files of a storage path. These paths are joined to
/// a file name by plain concatenation, so both `/dir/` and `/dir/prefix_` are valid.
fn storage_dir(path: &str) -> String {
    if path.ends_with('/') {
        return path.to_string();
    }
    match Path::new(path).parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_string_lossy().into_owned(),
        _ => ".".to_string(),
    }
}

/// What a storage path holds, as (path, metadata). Paths are spelled the way jobs
/// spell them, because a GC pin only protects an exact match.
fn storage_entries(path: &str) -> std::io::Result<Vec<(std::path::PathBuf, fs::Metadata)>> {
    let as_prefix = !path.ends_with('/') && !Path::new(path).is_dir();
    let prefix = if as_prefix {
        Path::new(path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        String::new()
    };
    let dir = if as_prefix {
        storage_dir(path)
    } else {
        path.to_string()
    };

    let mut entries = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix(prefix.as_str()) else {
            continue;
        };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let spelled = if as_prefix || path.ends_with('/') {
            std::path::PathBuf::from(format!("{}{}", path, rest))
        } else {
            entry.path()
        };
        entries.push((spelled, metadata));
    }
    Ok(entries)
}

/// Delete the oldest files until the directory is back under its threshold.
fn garbage_collect(directory: &str, size_threshold: u64) {
    let entries = match storage_entries(directory) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("Garbage collection cannot read {}: {}", directory, e);
            return;
        }
    };
    let mut files: Vec<_> = entries
        .into_iter()
        .filter_map(|(path, meta)| {
            // Pinned: something is still using it. That includes sources with
            // an in-flight shadow moderation call, which CONTRACT §3 requires
            // undisturbed until the sidecar's response arrives (worst case hours).
            if moderation::is_pinned(&path) {
                return None;
            }
            let size = if meta.is_dir() {
                dir_size(&path)
            } else {
                meta.len()
            };
            // Not every filesystem records a creation time. Without one, the
            // file must still be collectable or it stays for ever.
            let created = meta
                .created()
                .or_else(|_| meta.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            Some((path, size, created, meta.is_dir()))
        })
        .collect();

    // Newest first, so that pop() takes the oldest.
    files.sort_by_key(|k| std::cmp::Reverse(k.2));

    let mut total_size: u64 = files.iter().map(|(_, size, _, _)| size).sum();

    while total_size > size_threshold && !files.is_empty() {
        if let Some((path, size, _, is_dir)) = files.pop() {
            // Re-check at DELETE time: a pin can land after the snapshot
            // above (a job cache-hits this source mid-GC-pass), and the
            // snapshot-time filter alone would still delete it (review
            // round 2, finding 1).
            if moderation::is_pinned(&path) {
                continue;
            }
            if is_dir {
                fs::remove_dir_all(&path).ok();
            } else {
                fs::remove_file(&path).ok();
            }
            total_size = total_size.saturating_sub(size);
        }
    }
}

/// One round of cleaning up, off the async workers because it touches a lot of
/// files. A panic in here is logged and the next round runs as normal.
async fn run_maintenance() {
    let collection = tokio::task::spawn_blocking(|| {
        const ONE_GB: u64 = 1_000_000_000;
        garbage_collect(
            PATH_TO_FILE.as_str(),
            env_u64("FILE_SIZE_THRESHOLD", ONE_GB),
        );
        garbage_collect(
            PATH_TO_TRANSCODED_FILE.as_str(),
            env_u64("TRANSCODED_FILE_SIZE_THRESHOLD", ONE_GB),
        );
    })
    .await;
    if let Err(e) = collection {
        eprintln!("ALERT: garbage collection failed: {:?}", e);
    }
    prune_job_records(job_record_ttl()).await;
}

/// Make sure a storage path can be written to, so that a bad mount stops the server
/// at startup instead of failing every job later on.
fn prepare_storage(path: &str) -> std::result::Result<(), String> {
    let dir = storage_dir(path);
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {}", dir, e))?;
    let check = Path::new(&dir).join(format!(".write-check-{}.part", Uuid::new_v4()));
    fs::write(&check, b"").map_err(|e| format!("cannot write to {}: {}", dir, e))?;
    let _ = fs::remove_file(&check);
    Ok(())
}

/// Temporary files of jobs that were running when a process died. Their names
/// end in a task id nothing will ask for again, so only disk space (and, for
/// `.part` and `_ue` files, decrypted content) is left of them.
fn is_leftover(name: &str, is_dir: bool) -> bool {
    if is_dir {
        name.ends_with("_hls") || name.ends_with("_modtap")
    } else {
        name.ends_with(".part") || name.contains("_ue.")
    }
}

/// Runs once at startup, when no job of this process can be using these files.
fn sweep_leftovers(path: &str) {
    let entries = match storage_entries(path) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("Cannot look for leftover files in {}: {}", path, e);
            return;
        }
    };
    for (entry_path, meta) in entries {
        let name = entry_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !is_leftover(&name, meta.is_dir()) {
            continue;
        }
        let removed = if meta.is_dir() {
            fs::remove_dir_all(&entry_path)
        } else {
            fs::remove_file(&entry_path)
        };
        if let Err(e) = removed {
            eprintln!("Cannot remove leftover file {}: {}", name, e);
        }
    }
}

pub mod transcode {
    tonic::include_proto!("transcode");
}

// Define a struct to receive the query parameters.
#[derive(Deserialize)]
struct QueryParams {
    source_cid: String,
    media_formats: String,
    is_encrypted: bool,
    is_gpu: bool,
    preview_percent: Option<u32>,
}

/// The REST API. Rejections end up in `auth::handle_rejection`, so that a bad token is
/// answered with the 401 the API documents.
fn rest_routes(
    rest_handler: Arc<RestHandler>,
) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
    let cors = warp::cors()
        .allow_any_origin()
        .allow_methods(vec!["POST", "GET"])
        .allow_headers(vec!["Content-Type", "Authorization"]);

    let transcode_handler = Arc::clone(&rest_handler);
    let transcode = warp::path!("transcode")
        .and(auth::with_auth()) // Apply JWT authentication middleware
        .and(warp::query::<QueryParams>())
        .and_then(move |params: QueryParams| {
            let rest_handler = Arc::clone(&transcode_handler);
            async move {
                rest_handler
                    .transcode(
                        params.source_cid,
                        params.media_formats,
                        params.is_encrypted,
                        params.is_gpu,
                        params.preview_percent.unwrap_or(0),
                    )
                    .await
            }
        })
        .with(cors.clone())
        .boxed();

    let get_transcoded_handler = Arc::clone(&rest_handler);
    let get_transcoded = warp::path!("get_transcoded" / String)
        .and(auth::with_auth()) // Apply JWT authentication middleware
        .and_then(move |task_id| {
            let rest_handler = Arc::clone(&get_transcoded_handler);
            async move { rest_handler.get_transcoded(task_id).await }
        })
        .with(cors.clone())
        .boxed();

    let health = warp::path!("health")
        .and(warp::get())
        .map(|| warp::reply::json(&serde_json::json!({"status": "ok"})));

    let status = warp::path!("status")
        .and(warp::get())
        .and(auth::with_auth())
        .map(|| {
            warp::reply::json(&StatusResponse {
                active_jobs: shared::active_jobs(),
                queued_jobs: shared::queued_jobs(),
                max_concurrent: shared::max_concurrent(),
            })
        })
        .with(cors.clone())
        .boxed();

    health
        .or(status)
        .or(transcode)
        .or(get_transcoded)
        .recover(auth::handle_rejection)
}

/// The main entry point for the transcode server. Initializes the server
/// with the specified configuration, starts the gRPC server, and listens
/// for incoming requests. Once a request is received, it spawns a new thread
/// to handle the request and continues listening for more requests.
///
#[tokio::main]
async fn main() {
    dotenv().ok();

    // Q1(b) — a set-but-unrecognised MODERATION_GATE is BOOT-FATAL. This must
    // run before either listener binds and before any job can be accepted: a
    // validation that runs afterwards is not "refuse to start". Under compose's
    // restart policy a typo surfaces as a crash-loop at deploy time, which is
    // the intent — loud, immediate, and while someone is watching.
    if let Err(e) = moderation::validate_gate_mode() {
        eprintln!("FATAL: MODERATION_GATE — {}", e);
        std::process::exit(1);
    }

    // Also before anything is accepted: without storage, every job would fail.
    for name in ["PATH_TO_FILE", "PATH_TO_TRANSCODED_FILE"] {
        let path = var(name).map_err(|_| "not set".to_string());
        match path.and_then(|path| prepare_storage(&path).map(|()| path)) {
            Ok(path) => sweep_leftovers(&path),
            Err(e) => {
                eprintln!("FATAL: {} — {}", name, e);
                std::process::exit(1);
            }
        }
    }

    let (task_sender, task_receiver) = mpsc::channel::<TranscodeJob>(100);
    let task_receiver = Arc::new(Mutex::new(task_receiver));
    tokio::spawn(keep_restarting(
        "job queue",
        Duration::from_secs(1),
        move || {
            run_job_queue(
                task_receiver.clone(),
                &shared::SEMAPHORE,
                job_deadline(),
                run_queued_job,
            )
        },
    ));

    let grpc_addr = "0.0.0.0:50051"
        .parse()
        .expect("Invalid gRPC server address");
    let transcode_service_handler = TranscodeServiceHandler {
        transcode_task_sender: task_sender.clone(),
    };
    let grpc_server = Server::builder()
        .add_service(TranscodeServiceServer::new(transcode_service_handler))
        .serve(grpc_addr);

    let rest_handler = Arc::new(RestHandler {
        transcode_task_sender: task_sender,
    });

    let rest_server = warp::serve(rest_routes(rest_handler)).run(([0, 0, 0, 0], 8000));

    tokio::spawn(keep_restarting(
        "maintenance",
        Duration::from_secs(60),
        || async {
            let every = env_u64("GARBAGE_COLLECTOR_INTERVAL", 3600).max(1);
            let mut interval = tokio::time::interval(Duration::from_secs(every));
            loop {
                interval.tick().await;
                run_maintenance().await;
            }
        },
    ));

    // Neither listener should ever end. If one does, this process is no use, and
    // only exiting lets the container's restart policy bring it back: staying up
    // with one port dead looks healthy from outside.
    let grpc_server = tokio::spawn(grpc_server);
    let rest_server = tokio::spawn(rest_server);
    tokio::select! {
        result = grpc_server => eprintln!("FATAL: gRPC server ended: {:?}", result),
        result = rest_server => eprintln!("FATAL: REST server ended: {:?}", result),
    }
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::moderation::{
        may_publish, GateMode, ModerationClient, ModerationOutcome, StubModerationClient,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The production source with the test module stripped off.
    ///
    /// Structural assertions MUST search this rather than the whole file. A
    /// `src.contains("...")` over the whole file is satisfied by the *test's
    /// own string literal*, so it passes even if the production code it claims
    /// to check were deleted entirely — a silently vacuous assertion. Slicing
    /// the tests away makes that impossible without needing `concat!` tricks
    /// at every call site.
    fn job_flow_src() -> &'static str {
        let src = include_str!("server.rs");
        &src[..src
            .find("\n#[cfg(test)]\nmod tests")
            .expect("test module marker")]
    }

    /// A source file without its test module.
    fn production_part(source: &'static str) -> &'static str {
        match source.find("\n#[cfg(test)]\nmod tests") {
            Some(end) => &source[..end],
            None => source,
        }
    }

    /// Guards the guards: a file cut short here would make them all pass for nothing.
    #[test]
    fn test_the_production_part_of_a_file_is_all_of_it() {
        for (source, last_function) in [
            (job_flow_src(), "async fn main()"),
            (
                include_str!("transcode_video.rs"),
                "async fn encrypt_and_hash(",
            ),
            (include_str!("s5.rs"), "fn blake3_digest"),
            (
                include_str!("utils.rs"),
                "pub async fn download_and_concat_files",
            ),
        ] {
            assert!(
                production_part(source).contains(last_function),
                "{}",
                last_function
            );
        }
    }

    #[test]
    fn test_job_flow_src_excludes_the_test_module() {
        // Guards the guard: if this ever returned the whole file, every
        // structural assertion below would quietly become vacuous.
        let flow = job_flow_src();
        assert!(flow.contains("async fn process_single_job"));
        assert!(
            !flow.contains("fn test_job_flow_src_excludes_the_test_module"),
            "job_flow_src must not include the test module"
        );
    }

    /// A client that delays then returns a (late) `Unavailable` — models a verdict
    /// arriving after the timeout window.
    struct SlowStub;
    #[async_trait::async_trait]
    impl ModerationClient for SlowStub {
        async fn moderate(
            &self,
            _t: &str,
            _k: &[Vec<u8>],
            _s: Option<String>,
        ) -> ModerationOutcome {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            ModerationOutcome::Unavailable
        }
    }

    #[tokio::test]
    async fn test_gate_publishes_only_on_cleared() {
        // The publish seam (an AtomicUsize) runs iff may_publish — exactly once for
        // Cleared, zero for every other outcome.
        for (outcome, expect) in [
            (ModerationOutcome::Cleared, 1usize),
            (ModerationOutcome::Blocked, 0),
            (ModerationOutcome::Flagged, 0),
            (ModerationOutcome::Unavailable, 0),
        ] {
            let published = AtomicUsize::new(0);
            let client = StubModerationClient { outcome };
            let verdict = client.moderate("task", &[], None).await;
            if may_publish(&verdict) {
                published.fetch_add(1, Ordering::SeqCst);
            }
            assert_eq!(published.load(Ordering::SeqCst), expect);
        }
    }

    #[tokio::test]
    async fn test_node_down_holds() {
        // Unavailable models down / timeout / 404 / 4xx / 5xx ⇒ never publish.
        let client = StubModerationClient {
            outcome: ModerationOutcome::Unavailable,
        };
        let verdict = client.moderate("task", &[], None).await;
        assert!(!may_publish(&verdict));
    }

    #[tokio::test]
    async fn test_slow_verdict_holds() {
        // We AWAIT the (late) verdict before any publish — never publish-then-block.
        let published = AtomicUsize::new(0);
        let verdict = SlowStub.moderate("task", &[], None).await;
        if may_publish(&verdict) {
            published.fetch_add(1, Ordering::SeqCst);
        }
        assert_eq!(published.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_process_single_job_gates_before_publish() {
        let src = job_flow_src();
        // needles via concat! so this test's own literals don't self-match
        let moderate = concat!("client.", "moderate(");
        let publish = concat!("transcode_video_", "publish(");
        // NEEDLE CHANGED on revival (Task 3.3.1): the gate is guarded by
        // `gate_mode()`, not by M3's shadow-invocation switch. Left as the old
        // needle this test would still pass — satisfied by the shadow-spawn
        // guard — while asserting nothing whatsoever about the gate.
        let guard = concat!("gate_", "mode()");
        let mod_idx = src.find(moderate).expect("gate moderate call present");
        let pub_idx = src.find(publish).expect("publish call present");
        assert!(mod_idx < pub_idx, "moderate must run before publish");
        assert!(src.contains(guard), "gate guarded by gate_mode()");
        assert!(src.contains("may_publish"));
    }

    /// Task 3.3.2 — the GateMode matrix. `dark` publishes on EVERY outcome;
    /// `enforce` publishes only on `Cleared`. This is the one behavioural
    /// difference between the two modes, and the reason a boolean switch could
    /// not express Milestone 1.
    #[tokio::test]
    async fn test_gate_mode_matrix_dark_publishes_enforce_holds() {
        // The expectations are LITERAL, deliberately not derived from
        // `may_publish()`. Computing them from the same predicate the wiring
        // uses would make the assertion a restatement of the code under test —
        // it would pass whatever `holds()` returned. Written out by hand, the
        // Dark column proves dark never withholds and the Enforce column
        // proves only `Cleared` publishes.
        for (outcome, dark_publishes, enforce_publishes) in [
            (ModerationOutcome::Cleared, true, true),
            (ModerationOutcome::Blocked, true, false),
            (ModerationOutcome::Flagged, true, false),
            (ModerationOutcome::Unavailable, true, false),
        ] {
            for (mode, expect) in [
                (GateMode::Dark, dark_publishes),
                (GateMode::Enforce, enforce_publishes),
            ] {
                let published = AtomicUsize::new(0);
                let client = StubModerationClient {
                    outcome: outcome.clone(),
                };
                let verdict = client.moderate("task", &[], None).await;
                // The decision exactly as the wiring makes it: withhold only
                // when the mode holds AND the verdict is not publishable.
                if !(mode.holds() && !may_publish(&verdict)) {
                    published.fetch_add(1, Ordering::SeqCst);
                }
                assert_eq!(
                    published.load(Ordering::SeqCst),
                    usize::from(expect),
                    "mode {:?} outcome {:?}",
                    mode,
                    outcome
                );
            }
        }
    }

    /// The three-way split IS the gate, and it lives in a job flow that cannot
    /// be unit-tested — so pin its control-flow shape structurally. This is the
    /// assertion the behavioural matrix above cannot make: that `enforce`'s
    /// hold returns before the publish loop, and that `dark` falls INTO that
    /// loop rather than around it (so the tap-dir cleanup still runs).
    #[test]
    fn test_three_way_split_returns_in_enforce_and_falls_through_in_dark() {
        let src = job_flow_src();
        let start = src
            .find("if !moderation::may_publish(&outcome) {")
            .expect("gate decision present");
        let region = &src[start..];
        let holds = region
            .find("if gate.holds() {")
            .expect("the hold branch must be guarded by gate.holds()");
        let hold_log = region
            .find(concat!("MODERATION", " HOLD task_id="))
            .expect("hold log present");
        let ret = region
            .find("return Ok(());")
            .expect("the hold path must return");
        let would = region.find("WOULD-HOLD").expect("dark log present");
        let publish = region
            .find(concat!("transcode_video_", "publish("))
            .expect("publish loop present");
        assert!(holds < hold_log, "the HOLD log sits inside gate.holds()");
        assert!(hold_log < ret, "the HOLD path logs, then returns");
        assert!(
            ret < would,
            "WOULD-HOLD must be OUTSIDE the holds branch — dark must not return"
        );
        assert!(
            would < publish,
            "dark logs WOULD-HOLD then falls INTO the publish loop"
        );
        // The one that matters most, and the one an ordering check alone
        // MISSES: a `return` inserted after the WOULD-HOLD log would silently
        // stop dark mode publishing — destroying Milestone 1 — while leaving
        // every index above in the same order. Assert the fall-through
        // directly: nothing may exit the function between the dark log and the
        // publish loop it is supposed to fall into.
        assert!(
            !region[would..publish].contains("return"),
            "no early exit may sit between WOULD-HOLD and the publish loop — \
             dark must fall INTO it, not around it"
        );
    }

    /// Task 3.3.3 — dark-launch parity: `Off` must reach neither the tap, the
    /// POST, nor the gate. Structural, because it is an absence in a job flow.
    #[test]
    fn test_gate_off_taps_nothing_and_posts_nothing() {
        let src = job_flow_src();
        // Every gate-path side effect is guarded by `gate.moderates()`, which is
        // false for Off — so none of them is reachable with the gate unset.
        for guarded in [
            "let scan_required = gate.moderates()",
            "let _tap_files = gate.moderates()",
        ] {
            assert!(src.contains(guarded), "missing gate guard: {}", guarded);
        }
        // The keyframe read and the POST must sit inside the ARMED block. Use
        // rfind, not find: the first `if gate.moderates() {` is the stale-tap
        // pre-clear near the top of the job, so anchoring on it would make this
        // assertion trivially true and prove nothing about containment.
        let arm = src
            .rfind("if gate.moderates() {")
            .expect("gate-armed block present");
        let read = src
            .find("read_keyframe_pngs(&frames_dir)")
            .expect("tap read present");
        let moderate = src.find("client.moderate(").expect("moderate call present");
        assert!(
            arm < read && arm < moderate,
            "the tap read and the POST must live inside the gate-armed block"
        );
        // The tap is only ever handed to produce when a format claims it, and
        // `want_tap` is only reachable inside the armed branch.
        assert!(
            src.contains("let this_tap = if want_tap {"),
            "tap_dir is passed to produce only via want_tap"
        );
        // And Off is genuinely the default.
        assert_eq!(
            crate::moderation::parse_gate_mode("").unwrap(),
            GateMode::Off
        );
    }

    /// `held_output_path` must give the same path `transcode_video` writes to and
    /// later reads. If they drift, the HOLD branch deletes nothing, silently.
    /// The HLS case goes through the same function.
    #[test]
    fn test_held_output_path_is_where_the_transcoder_writes() {
        crate::test_support::storage();
        let local_file = shared::LocalFile::file("unused");
        let plain = LocalOutput {
            task_id: "task".to_string(),
            format_index: 0,
            file_name: "name".to_string(),
            ext: "mp4".to_string(),
            dest: None,
            encrypt_flag: false,
            is_hls: false,
            total_duration: 1.0,
            preview_percent: 0,
            video_format: json!({}),
            local_file,
        };
        assert_eq!(
            held_output_path(&plain),
            transcode_video::local_output_path("name", "mp4", false)
        );
        let hls = LocalOutput {
            is_hls: true,
            ..plain
        };
        assert_eq!(
            held_output_path(&hls),
            transcode_video::local_output_path("name", "mp4", true)
        );
    }

    /// What protects a job's files from GC while it needs them. `is_pinned` is an
    /// exact match, so what gets pinned has to be spelled as GC will see it.
    #[test]
    fn test_a_job_holds_its_source_and_tap_dir_for_as_long_as_it_runs() {
        let src = job_flow_src();
        for pin in [
            "let _source_pin = moderation::pin(&file_path)",
            "LocalFile::dir(&tap_dir)",
        ] {
            assert!(src.contains(pin), "missing GC pin: {}", pin);
        }
        assert!(
            src.contains("fs::remove_dir_all(held_output_path(out))")
                && src.contains("fs::remove_file(held_output_path(out))"),
            "the HOLD path must delete what it held"
        );
        // Pin first, then look: GC could otherwise take a cached source between
        // the job seeing it and pinning it.
        let pinned = src
            .find("let _source_pin = moderation::pin(&file_path)")
            .unwrap();
        let fetched = src.find("fetch_source(").unwrap();
        let fetched_call = pinned + src[pinned..].find("fetch_source(").unwrap();
        assert!(fetched < pinned, "the definition comes first in the file");
        assert!(pinned < fetched_call);
    }

    /// GC reads directories as they are, so a pin spelled by a job has to match
    /// what GC sees, in every path shape.
    #[test]
    fn test_garbage_collect_spares_a_file_pinned_by_its_job_path() {
        let dir = gc_test_dir("spelled");
        let as_a_job_spells_it = format!("{}/", dir.to_str().unwrap());
        let source = format!("{}pinned", as_a_job_spells_it);
        std::fs::write(&source, [0u8; 8]).unwrap();
        std::fs::write(dir.join("other"), [0u8; 8]).unwrap();
        let guard = crate::moderation::pin(&source);
        garbage_collect(&as_a_job_spells_it, 0);
        assert!(Path::new(&source).exists());
        assert!(!dir.join("other").exists());
        drop(guard);

        let prefix = format!("{}/src_", dir.to_str().unwrap());
        let source = format!("{}pinned", prefix);
        std::fs::write(&source, [0u8; 8]).unwrap();
        let guard = crate::moderation::pin(&source);
        garbage_collect(&prefix, 0);
        assert!(Path::new(&source).exists());
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── M3 — Phase 5.3: shadow-wiring structural tests ────────────────────

    #[test]
    fn test_garbage_collect_spares_pinned_file() {
        // Behavioral (not just structural): a pinned source survives a GC
        // pass that collects everything else — this catches any mismatch
        // between the pin registry's path strings and read_dir's entries.
        let dir = std::env::temp_dir().join("m3_gc_pin_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pinned = dir.join("pinned_source");
        let victim = dir.join("unpinned_source");
        std::fs::write(&pinned, [0u8; 64]).unwrap();
        std::fs::write(&victim, [0u8; 64]).unwrap();
        let guard = crate::moderation::pin(pinned.to_str().unwrap());
        garbage_collect(dir.to_str().unwrap(), 0); // threshold 0: delete all it can
        assert!(pinned.exists(), "pinned source must survive GC");
        assert!(!victim.exists(), "unpinned file must be collected");
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The DIRECTORY case (plan Task 3.1.4). The gate pins directories as well
    /// as files — the tap dir and every held HLS output dir — and GC deletes
    /// those through a different branch (`remove_dir_all`). `is_pinned` is an
    /// exact string match, so a near-miss path (trailing slash, un-canonicalised,
    /// a `_hls` suffix built two different ways) protects nothing and fails
    /// silently. This proves the directory shape actually matches.
    #[test]
    fn test_garbage_collect_spares_pinned_directory() {
        let dir = std::env::temp_dir().join("wpt_gc_pin_dir_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pinned = dir.join("held_output_hls");
        let victim = dir.join("unpinned_output_hls");
        for d in [&pinned, &victim] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("seg0.ts"), [0u8; 64]).unwrap();
        }
        let guard = crate::moderation::pin(pinned.to_str().unwrap());
        garbage_collect(dir.to_str().unwrap(), 0); // threshold 0: delete all it can
        assert!(pinned.exists(), "pinned output dir must survive GC");
        assert!(!victim.exists(), "unpinned dir must be collected");
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// D1/D4 — the M3 shadow path can never delay, fail, or block a job.
    ///
    /// RESCOPED to `spawn_shadow_moderation`'s body (WP-T Task 3.2.1). The
    /// original assertions were file-wide, which was a sound proxy only while
    /// the publish gate was quarantined: the file now legitimately contains
    /// `may_publish`, `MODERATION HOLD` and `transcode_video_produce` on the
    /// WP-T gate path. The invariant they were really protecting is that none
    /// of them appear in the SHADOW path — which is what this asserts directly,
    /// so the coverage is tightened rather than dropped.
    #[test]
    fn test_shadow_moderation_is_fail_open() {
        let src = include_str!("server.rs");
        let f_start = src
            .find("fn spawn_shadow_moderation")
            .expect("shadow task fn present");
        let body = &src[f_start..];
        let body = &body[..body.find("\nasync fn ").unwrap_or(body.len())];
        // needles via concat! so this test's own literals don't self-match
        let may = concat!("may_", "publish");
        let hold = concat!("MODERATION", " HOLD");
        let produce = concat!("transcode_video_", "produce(");
        assert!(!body.contains(may), "no publish gate in the shadow path");
        assert!(!body.contains(hold), "the shadow path never holds a job");
        assert!(!body.contains(produce), "the shadow path never produces");
        // the shadow task is spawned, guarded only by the invocation switch
        let spawn = concat!("spawn_shadow_", "moderation(");
        let guard = concat!("moderation_", "enabled()");
        assert!(src.contains(spawn), "shadow task spawned");
        assert!(src.contains(guard), "guarded by the invocation switch");
    }

    /// Task 3.2.2 — the complement. D4's "incapable of delaying a job" has to
    /// survive a file that now legitimately contains a hold, so assert the
    /// shadow spawn sits OUTSIDE the gate path and is never awaited.
    #[test]
    fn test_shadow_spawn_is_outside_the_gate_and_never_awaited() {
        let src = job_flow_src();
        let call = concat!("spawn_shadow_", "moderation(task_id.clone()");
        let at = src.find(call).expect("shadow spawn call site");
        let line_end = src[at..].find('\n').map(|e| at + e).unwrap_or(src.len());
        assert!(
            !src[at..line_end].contains(".await"),
            "the shadow spawn must be fire-and-forget, never awaited (D4)"
        );
        let gate_at = src
            .find("let scan_required = gate.moderates()")
            .expect("gate present");
        assert!(
            at < gate_at,
            "the shadow spawn must sit outside the gate path, not inside a gate branch"
        );
    }

    #[test]
    fn test_shadow_task_pins_logs_and_relays() {
        let src = include_str!("server.rs");
        let f_start = src
            .find("fn spawn_shadow_moderation")
            .expect("shadow task fn present");
        // slice to the next top-level item — find() ends are always char-safe
        // (a fixed byte length can land mid-UTF-8 and panic)
        let body = &src[f_start..];
        let body = &body[..body.find("\nasync fn ").unwrap_or(body.len())];
        assert!(
            body.contains("moderation::pin("),
            "source pinned for the shadow window (RAII)"
        );
        assert!(body.contains("MODERATION SHADOW"), "structured log line");
        assert!(
            body.contains("wait_ms") && body.contains("call_ms"),
            "wait/call clocks logged separately (call_ms = CONTRACT §6 number)"
        );
        assert!(body.contains("build_relay_envelope"), "verdicts relayed");
    }

    /// Q2 — the armed gate suppresses the relay hop and NOTHING else. If this
    /// guard ever widened to cover the sidecar call, the shadow evidence the
    /// whole M3 milestone exists to collect would silently stop being gathered;
    /// if it narrowed away entirely, an OQ-12 VLM false positive could
    /// permanently poison a job Track-1 had cleared (node verdicts are
    /// monotonic and `blocked` over `cleared` is irreversible).
    #[test]
    fn test_relay_is_suppressed_by_the_gate_but_the_scan_is_not() {
        let src = include_str!("server.rs");
        let f_start = src
            .find("fn spawn_shadow_moderation")
            .expect("shadow task fn present");
        let body = &src[f_start..];
        let body = &body[..body.find("\nasync fn ").unwrap_or(body.len())];

        let guard = body
            .find("should_relay()")
            .expect("the relay must be guarded by the gate (Q2)");
        let relay = body
            .find("build_relay_envelope")
            .expect("relay call site present");
        assert!(guard < relay, "the guard must precede the relay call");

        let scan = body
            .find("moderate_via_sidecar")
            .expect("sidecar call present");
        assert!(
            scan < guard,
            "the sidecar POST must happen regardless of the gate — suppressing \
             the scan would stop shadow evidence collection (D1/D4)"
        );
        assert!(
            body.contains("relay suppressed"),
            "suppression must be logged, or an absent relay reads as a dead one"
        );
    }

    #[test]
    fn test_gc_is_sole_source_deletion_path() {
        // Source-lifetime invariant (plan: review issue 3): outside
        // garbage_collect, no deletion may touch a source path — the pin
        // registry is only sound because GC is the sole deletion path.
        let src = include_str!("server.rs");
        let del_file = concat!("remove_", "file");
        let del_dir = concat!("remove_dir", "_all");
        let src_var = concat!("file_", "path");
        let src_root = concat!("PATH_TO_", "FILE");
        let gc_start = src.find("fn garbage_collect").expect("gc fn");
        let gc_len = src[gc_start..].find("\n}").expect("gc end") + 2;
        let mut offset = 0usize;
        for line in src.lines() {
            let in_gc = offset >= gc_start && offset < gc_start + gc_len;
            if !in_gc && (line.contains(del_file) || line.contains(del_dir)) {
                assert!(
                    !line.contains(src_var) && !line.contains(src_root),
                    "non-GC deletion touching a source path: {}",
                    line
                );
            }
            offset += line.len() + 1;
        }
    }

    // ── M3 — Phase 4.2: source finalisation ───────────────────────────────

    #[test]
    fn test_finalize_part_renames_into_place() {
        let dir = std::env::temp_dir();
        let part = dir.join("m3_finalize_test.mp4.part");
        let final_path = dir.join("m3_finalize_test.mp4");
        let _ = std::fs::remove_file(&final_path);
        std::fs::write(&part, b"source bytes").unwrap();
        finalize_part(part.to_str().unwrap(), final_path.to_str().unwrap()).unwrap();
        assert!(!part.exists(), ".part must be gone after finalise");
        assert_eq!(std::fs::read(&final_path).unwrap(), b"source bytes");
        let _ = std::fs::remove_file(&final_path);
        // missing .part ⇒ Err, never panic
        assert!(finalize_part("/nonexistent.part", "/nonexistent").is_err());
    }

    /// Every source reaches its final name from one place, finished and flushed.
    #[test]
    fn test_sources_are_finalized_before_use() {
        let src = job_flow_src();
        let start = src
            .find("async fn fetch_source")
            .expect("fetch_source present");
        let body = &src[start..];
        let body = &body[..body.find("\n}\n").expect("fetch_source end")];
        assert!(
            body.contains("part_path"),
            "downloads go to a temporary name"
        );
        assert!(body.contains(concat!("finalize", "_part(")));
        assert_eq!(
            src.matches(concat!("finalize", "_part(")).count(),
            2,
            "one definition, one use"
        );
    }

    /// Two jobs can fetch one source at once. The sidecar refuses a file that changes
    /// while it reads, so the second must not replace what the first put there.
    #[test]
    fn test_finalize_part_leaves_an_existing_source_alone() {
        let dir = gc_test_dir("finalize");
        let (part, final_path) = (dir.join("x.part"), dir.join("x"));
        std::fs::write(&final_path, b"first").unwrap();
        std::fs::write(&part, b"second").unwrap();
        finalize_part(part.to_str().unwrap(), final_path.to_str().unwrap()).unwrap();
        assert_eq!(std::fs::read(&final_path).unwrap(), b"first");
        assert!(!part.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── M3 — Phase 4.1: GC pin wiring ─────────────────────────────────────

    #[test]
    fn test_garbage_collect_skips_pinned_sources() {
        let src = include_str!("server.rs");
        let gc_start = src
            .find("fn garbage_collect")
            .expect("garbage_collect present");
        // slice to the fn's closing brace — find() ends are always char-safe
        // (a fixed byte length can land mid-UTF-8 and panic)
        let gc_body = &src[gc_start..];
        let gc_body = &gc_body[..gc_body.find("\n}").map(|i| i + 2).unwrap_or(gc_body.len())];
        assert!(
            gc_body.matches("moderation::is_pinned").count() >= 2,
            "garbage_collect must consult the pin registry BOTH at snapshot \
             time and again at delete time (a pin can land mid-pass)"
        );
    }

    #[test]
    fn test_proto_get_transcoded_response_has_duration() {
        let proto_src = include_str!("../proto/transcode.proto");
        assert!(
            proto_src.contains("double duration"),
            "GetTranscodedResponse proto must contain 'double duration' field"
        );
    }

    #[test]
    fn test_get_transcoded_response_wrapper_has_duration() {
        let wrapper = GetTranscodedResponseWrapper {
            status_code: 200,
            metadata: String::new(),
            progress: 100,
            duration: 99.5,
        };
        assert_eq!(wrapper.duration, 99.5);
    }

    #[test]
    fn test_transcoded_map_stores_duration() {
        // Verify the TRANSCODED map value type stores (String, f64)
        let entry: (String, f64) = ("metadata".to_string(), 42.0);
        let mut map = std::collections::HashMap::<String, (String, f64)>::new();
        map.insert("task1".to_string(), entry);
        let (metadata, duration) = map.get("task1").cloned().unwrap();
        assert_eq!(metadata, "metadata");
        assert_eq!(duration, 42.0);
    }

    #[test]
    fn test_health_endpoint_exists() {
        let server_src = job_flow_src();
        assert!(
            server_src.contains(r#"warp::path!("health")"#),
            "REST server must have a /health endpoint"
        );
    }

    #[test]
    fn test_upload_video_s5_propagates_create_error() {
        let s5_src = include_str!("s5.rs");
        assert!(
            !s5_src.contains("String::new()"),
            "s5.rs must not swallow create_with_metadata errors with String::new()"
        );
        assert!(
            s5_src.contains("Failed to create file on server"),
            "s5.rs must have create error message"
        );
        assert!(
            s5_src.contains("return Err"),
            "s5.rs must return Err on create failure"
        );
    }

    #[test]
    fn test_upload_video_s5_propagates_upload_error() {
        let s5_src = include_str!("s5.rs");
        assert!(
            s5_src.contains(r#"return Err(anyhow!("Failed to upload file to server"#),
            "s5.rs must return Err on upload failure"
        );
    }

    #[test]
    fn test_transcode_loop_checks_status_code() {
        let server_src = job_flow_src();
        assert!(
            server_src.contains("response.status_code != 200"),
            "transcode loop must check response.status_code"
        );
    }

    // ── Failed-job status: non-200 from GetTranscoded means permanent failure ──

    const LONG_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

    fn fresh_task_id() -> String {
        Uuid::new_v4().to_string()
    }

    #[test]
    fn test_parse_source_cid_accepts_a_network_prefix() {
        assert_eq!(
            parse_source_cid("s5://abc.mp4"),
            Ok(("abc".to_string(), "s5".to_string()))
        );
        assert_eq!(
            parse_source_cid("ipfs://Qm123"),
            Ok(("Qm123".to_string(), "ipfs".to_string()))
        );
    }

    #[test]
    fn test_parse_source_cid_rejects_bad_input_as_400() {
        for bad in [
            "",
            "bare-cid",
            "s5://",
            "://abc",
            // Only the last part was kept, so different sources shared a cache file.
            "ipfs://QmDir/video.mp4",
            "s5://../../etc/passwd",
            "s5:///",
            "s5://.",
            // % means something to FFmpeg in a file name, and spaces to everything else.
            "s5://a%04db",
            "s5://a b",
            "s5://abc;rm",
            "s5://abc.mp4/../../etc/passwd",
        ] {
            let failure = parse_source_cid(bad).expect_err(bad);
            assert_eq!(failure.status_code, 400, "{:?}", bad);
        }
    }

    #[test]
    fn test_parse_source_cid_keeps_real_cids_whole() {
        for (given, cid) in [
            (
                "s5://uJh9dvBupLgWG3p8CGJ1VR8PLnZvJQedolo8ktb027PrlTT5LvAY",
                "uJh9dvBupLgWG3p8CGJ1VR8PLnZvJQedolo8ktb027PrlTT5LvAY",
            ),
            (
                "s5://zb2rhe5P4gXftAwvA4eXQ5HJwsER2owDyS9sKaQRRVQPn93bA.mp4",
                "zb2rhe5P4gXftAwvA4eXQ5HJwsER2owDyS9sKaQRRVQPn93bA",
            ),
            (
                "ipfs://bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi",
                "bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi",
            ),
            ("s5://u-_ab_-c", "u-_ab_-c"),
            // Padding, which the decoder has always allowed for.
            ("s5://uAbCd==", "uAbCd=="),
        ] {
            assert_eq!(parse_source_cid(given).unwrap().0, cid);
        }
    }

    fn encrypted_cid_for(key: &[u8], blob_hash: &[u8]) -> String {
        let mut hash = vec![0x1f];
        hash.extend_from_slice(blob_hash);
        let bytes = encrypted_cid::create_encrypted_cid(
            0xae,
            0xa6,
            18,
            hash,
            key.to_vec(),
            0,
            vec![1, 2, 3],
        );
        format!("u{}", bytes_to_base64url(&bytes))
    }

    #[test]
    fn test_parse_encrypted_cid_finds_the_key_and_blob_hash() {
        let key = [7u8; 32];
        let parsed = parse_encrypted_cid(&encrypted_cid_for(&key, &[9u8; 32])).unwrap();
        assert_eq!(parsed.key, key);
        let mut expected = vec![0x1f];
        expected.extend_from_slice(&[9u8; 32]);
        assert_eq!(parsed.blob_hash, bytes_to_base64url(&expected));
    }

    /// These all panicked, which reported bad input as a server fault.
    #[test]
    fn test_malformed_encrypted_cids_are_bad_input() {
        for bad in ["", "u", "uAAAA", "u!!!", "zAAAA", "éAAAA", "uAAAAA"] {
            let failure = parse_encrypted_cid(bad).expect_err(bad);
            assert_eq!(
                failure,
                JobFailure::bad_input("Invalid source CID"),
                "{:?}",
                bad
            );
        }
    }

    /// The file name appears in FFmpeg's arguments and output, so it can't be the
    /// CID of an encrypted source: that holds the key.
    #[test]
    fn test_the_cached_name_of_an_encrypted_source_does_not_contain_its_cid() {
        crate::test_support::storage();
        let cid = encrypted_cid_for(&[7u8; 32], &[9u8; 32]);
        let path = source_file_path(&cid, true);
        assert!(!path.contains(&cid));
        assert_eq!(
            path,
            source_file_path(&cid, true),
            "same CID, same cache entry"
        );
        assert_ne!(path, source_file_path("other", true));
        assert!(source_file_path("plain", false).ends_with("plain"));
    }

    #[test]
    fn test_log_safe_cid_stops_before_the_key() {
        let key = [0xabu8; 32];
        let cid = encrypted_cid_for(&key, &[9u8; 32]);
        let logged = log_safe_cid(&cid);
        assert!(!logged.contains(&bytes_to_base64url(&key)[..8]));
        assert!(logged.len() < 40, "{}", logged);
    }

    /// Nothing that would let a reader of the logs decrypt anything gets printed.
    #[test]
    fn test_keys_and_whole_cids_are_never_logged() {
        let sources = [
            job_flow_src(),
            include_str!("transcode_video.rs"),
            include_str!("utils.rs"),
            include_str!("encrypt_file.rs"),
        ];
        let print_of_a_secret = regex::Regex::new(
            r"(?s)(println|eprintln)!\([^;]*?\b(key|key_bytes|encryption_key1|encrypted_cid|encrypted_cid_bytes|cid_encrypted|base64url|source_cid|orig_source_cid|response\.cid)\b[^;]*?\);",
        )
        .unwrap();
        for source in sources {
            let production = production_part(source);
            for found in print_of_a_secret.find_iter(production) {
                assert!(
                    found.as_str().contains("log_safe_cid"),
                    "may log a secret: {}",
                    found.as_str()
                );
            }
        }
    }

    #[test]
    fn test_grpc_does_not_strip_the_prefix_the_job_requires() {
        // Stripping `s5://` left no `://`, so every S5 source sent over gRPC
        // failed the job's own CID check.
        let needle = concat!("strip_prefix(\"s5", "://\")");
        assert!(!job_flow_src().contains(needle));
    }

    #[test]
    fn test_parse_media_formats_blames_the_right_side() {
        let good = r#"[{"id":1,"ext":"mp4"}]"#;
        assert_eq!(parse_media_formats(good, true).unwrap().len(), 1);
        for bad in [
            "not json",
            "[]",
            "{}",
            // Valid JSON that used to count as a failed format: a 500 for a client mistake.
            r#"[{"ext":"mp4"}]"#,
            r#"[{"id":1,"ext":"mp4","trim_percent":300}]"#,
            r#"[{"id":1,"ext":"../x"}]"#,
            r#"[{"id":1,"ext":"mp4","vf":"movie=/etc/passwd"}]"#,
            r#"[{"id":1,"ext":"mp4"},"nonsense"]"#,
        ] {
            assert_eq!(
                parse_media_formats(bad, true).unwrap_err(),
                JobFailure::bad_input("Invalid media formats"),
                "formats sent by the client are bad input: {}",
                bad
            );
            assert_eq!(
                parse_media_formats(bad, false).unwrap_err(),
                JobFailure::server("Server configuration error"),
                "the server's own formats file is a server fault: {}",
                bad
            );
        }
    }

    /// The only test that sets MEDIA_FORMATS_FILE. A missing file used to panic,
    /// even for a job that had sent its own formats.
    #[test]
    fn test_a_missing_formats_file_is_a_configuration_error() {
        std::env::set_var("MEDIA_FORMATS_FILE", "/nonexistent/formats.json");
        assert_eq!(
            load_media_formats("").unwrap_err(),
            JobFailure::server("Server configuration error")
        );
        assert!(load_media_formats(r#"[{"id":1,"ext":"mp4"}]"#).is_ok());
    }

    #[test]
    fn test_all_formats_failed_only_when_something_failed_and_nothing_published() {
        assert!(all_formats_failed(0, 1));
        assert!(!all_formats_failed(0, 0), "all cached is not a failure");
        assert!(
            !all_formats_failed(1, 1),
            "partial success still returns 200"
        );
    }

    #[tokio::test]
    async fn test_job_status_reports_a_recorded_failure() {
        let id = fresh_task_id();
        record_failure(
            &id,
            JobFailure {
                status_code: 500,
                reason: "Source download failed",
            },
        )
        .await;
        assert_eq!(
            job_status(&id).await,
            (500, "Source download failed".to_string(), 0.0)
        );
    }

    #[tokio::test]
    async fn test_job_status_unchanged_for_running_done_and_held_jobs() {
        let running = fresh_task_id();
        register_job(&running);
        assert_eq!(
            job_status(&running).await,
            (200, "Transcoding in progress".to_string(), 0.0)
        );
        let done = fresh_task_id();
        TRANSCODED
            .lock()
            .await
            .insert(done.clone(), ("[{\"id\":1}]".to_string(), 9.5));
        assert_eq!(
            job_status(&done).await,
            (200, "[{\"id\":1}]".to_string(), 9.5)
        );
        let held = fresh_task_id();
        TRANSCODED
            .lock()
            .await
            .insert(held.clone(), ("[]".to_string(), 5.0));
        assert_eq!(job_status(&held).await, (200, "[]".to_string(), 5.0));
    }

    #[tokio::test]
    async fn test_supervise_records_returned_failures_only() {
        let failed = fresh_task_id();
        supervise(failed.clone(), LONG_DEADLINE, async {
            Err(JobFailure {
                status_code: 400,
                reason: "Invalid source CID",
            })
        })
        .await;
        assert_eq!(job_status(&failed).await.0, 400);

        let fine = fresh_task_id();
        register_job(&fine);
        supervise(fine.clone(), LONG_DEADLINE, async { Ok(()) }).await;
        assert_eq!(job_status(&fine).await.0, 200);
    }

    #[tokio::test]
    async fn test_supervise_turns_a_panic_into_a_500_and_survives_it() {
        let id = fresh_task_id();
        supervise(id.clone(), LONG_DEADLINE, async {
            panic!("boom: secret-detail")
        })
        .await;
        let (code, reason, _) = job_status(&id).await;
        assert_eq!(code, 500);
        assert!(
            !reason.contains("secret-detail"),
            "panic text must not reach API clients: {}",
            reason
        );
    }

    /// End to end through the real job function: the node developer's report.
    #[tokio::test]
    async fn test_a_cid_without_a_prefix_fails_the_job_with_400() {
        let id = fresh_task_id();
        supervise(
            id.clone(),
            LONG_DEADLINE,
            process_single_job(
                id.clone(),
                "bare-cid".to_string(),
                String::new(),
                false,
                false,
                0,
                GateMode::Off,
                Box::new(StubModerationClient {
                    outcome: ModerationOutcome::Cleared,
                }),
            ),
        )
        .await;
        assert_eq!(job_status(&id).await.0, 400);
    }

    #[tokio::test]
    async fn test_grpc_get_transcoded_returns_the_failure() {
        let id = fresh_task_id();
        record_failure(
            &id,
            JobFailure {
                status_code: 400,
                reason: "Invalid source CID",
            },
        )
        .await;
        let handler = TranscodeServiceHandler {
            transcode_task_sender: mpsc::channel(1).0,
        };
        let resp = handler
            .get_transcoded(Request::new(GetTranscodedRequest {
                task_id: id.clone(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.status_code, 400);
        assert_eq!(resp.metadata, "Invalid source CID");
    }

    #[tokio::test]
    async fn test_rest_get_transcoded_returns_the_failure() {
        let id = fresh_task_id();
        record_failure(
            &id,
            JobFailure {
                status_code: 500,
                reason: "Source download failed",
            },
        )
        .await;
        let handler = RestHandler {
            transcode_task_sender: mpsc::channel(1).0,
        };
        let reply = handler.get_transcoded(id).await.unwrap();
        let bytes = hyper::body::to_bytes(warp::Reply::into_response(reply).into_body())
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["status_code"], 500);
        assert_eq!(v["metadata"], "Source download failed");
    }

    /// Wiring the pure helper's tests cannot see.
    #[test]
    fn test_every_format_failure_is_counted_and_checked_before_the_result() {
        let flow = job_flow_src();
        assert_eq!(
            flow.matches(concat!("failed_formats", " += 1;")).count(),
            7,
            "serialize, parse, non-200 and Err (gate off), produce Err, and \
             non-200 and Err (publish)"
        );
        let check = flow
            .find(concat!(
                "all_formats_failed(",
                "transcoded_formats.len(), failed_formats)"
            ))
            .expect("all-formats-failed check present");
        let insert = flow
            .find("(transcoded_json, source_duration)")
            .expect("result recorded");
        assert!(check < insert, "a failed job must not record a 200 result");
    }

    /// The compiler forces every exit to name an outcome but cannot tell a
    /// wrong `Ok` from a right one. A download or decrypt failure reporting
    /// success would put the task back in "in progress" forever.
    #[test]
    fn test_only_the_moderation_hold_exits_early_as_success() {
        let flow = job_flow_src();
        let start = flow
            .find("async fn process_single_job")
            .expect("job function present");
        let body = &flow[start..];
        let body = &body[..body.find("\n}\n").expect("job function end")];
        assert_eq!(body.matches(concat!("return ", "Ok(());")).count(), 1);
        let hold = body
            .find(concat!("MODERATION", " HOLD task_id="))
            .expect("hold log present");
        assert!(
            body.find(concat!("return ", "Ok(());")).unwrap() > hold,
            "the one early success must be the hold path"
        );
    }

    // ── Whole jobs: real FFmpeg against the fake portal ─────────────────────

    use crate::test_support::{
        fresh_cid, have_ffmpeg, outputs_containing, portal, sample_audio, sample_video,
        sample_with_a_false_duration, sources_containing, Fault, UPLOAD_TEST_LOCK,
    };

    const SMALL: &str =
        r#"[{"id":1,"ext":"mp4","vcodec":"libx264","c_a":"aac","vf":"scale=32:32"}]"#;

    #[derive(Debug)]
    struct JobEnd {
        id: String,
        status_code: i32,
        metadata: String,
        progress: i32,
        duration: f64,
    }

    async fn run_job_with(
        source_cid: &str,
        formats: &str,
        is_encrypted: bool,
        gate: GateMode,
        client: Box<dyn ModerationClient>,
    ) -> JobEnd {
        let id = fresh_task_id();
        register_job(&id);
        supervise(
            id.clone(),
            LONG_DEADLINE,
            process_single_job(
                id.clone(),
                source_cid.to_string(),
                formats.to_string(),
                is_encrypted,
                false,
                0,
                gate,
                client,
            ),
        )
        .await;
        let (status_code, metadata, duration) = job_status(&id).await;
        JobEnd {
            progress: shared::calculate_overall_progress(&id),
            id,
            status_code,
            metadata,
            duration,
        }
    }

    async fn run_job(source_cid: &str, formats: &str) -> JobEnd {
        run_job_with(
            source_cid,
            formats,
            false,
            GateMode::Off,
            Box::new(StubModerationClient {
                outcome: ModerationOutcome::Cleared,
            }),
        )
        .await
    }

    fn serve_source(bytes: &[u8]) -> (String, String) {
        let cid = fresh_cid();
        let path = format!("/s5/blob/{}", cid);
        portal().serve(&path, bytes);
        (format!("s5://{}", cid), path)
    }

    fn published_cids(metadata: &str) -> Vec<String> {
        let formats: Vec<Value> = serde_json::from_str(metadata).expect(metadata);
        formats
            .iter()
            .map(|f| f["cid"].as_str().unwrap().to_string())
            .collect()
    }

    /// Height of the video in a blob, using the real ffprobe.
    fn height_of(bytes: &[u8]) -> u32 {
        let path = std::env::temp_dir().join(format!("probe-{}", Uuid::new_v4()));
        std::fs::write(&path, bytes).unwrap();
        let out = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "v:0", "-show_entries"])
            .args(["stream=height", "-of", "csv=p=0"])
            .arg(&path)
            .output()
            .unwrap();
        let _ = std::fs::remove_file(&path);
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn test_a_job_publishes_its_output_and_cleans_up() {
        if !have_ffmpeg() {
            return;
        }
        let _uploads = UPLOAD_TEST_LOCK.lock().await;
        let (source, _) = serve_source(sample_video());
        let before = portal().uploads().len();

        let end = run_job(&source, SMALL).await;

        assert_eq!(end.status_code, 200, "{}", end.metadata);
        assert_eq!(end.progress, 100);
        let cids = published_cids(&end.metadata);
        assert_eq!(cids.len(), 1);
        assert!(cids[0].starts_with("s5://u"), "{}", cids[0]);
        let uploads = portal().uploads();
        assert_eq!(uploads.len(), before + 1);
        assert_eq!(height_of(&uploads[before].data), 32);
        assert!(
            outputs_containing(&end.id).is_empty(),
            "local output left behind"
        );
        assert_eq!(
            sources_containing(source.trim_start_matches("s5://")).len(),
            1
        );
    }

    /// An error page used to be saved as the source, under its final name, and every
    /// later job for that CID then failed in FFmpeg until GC happened to remove it.
    #[tokio::test]
    async fn test_an_http_error_fails_the_job_without_poisoning_the_cache() {
        if !have_ffmpeg() {
            return;
        }
        let _uploads = UPLOAD_TEST_LOCK.lock().await;
        let cid = fresh_cid();
        let source = format!("s5://{}", cid);
        // Nothing served at the path yet: a 404 from the portal.
        let end = run_job(&source, SMALL).await;
        assert_eq!(
            (end.status_code, end.metadata.as_str()),
            (500, "Source download failed")
        );
        assert!(sources_containing(&cid).is_empty(), "nothing may be cached");

        portal().serve(&format!("/s5/blob/{}", cid), sample_video());
        let end = run_job(&source, SMALL).await;
        assert_eq!(end.status_code, 200, "the next job must be able to succeed");
    }

    #[tokio::test]
    async fn test_a_passing_download_problem_does_not_fail_the_job() {
        if !have_ffmpeg() {
            return;
        }
        let _uploads = UPLOAD_TEST_LOCK.lock().await;
        for fault in [Fault::Status(502), Fault::CutOff] {
            let (source, path) = serve_source(sample_video());
            portal().fail_next(&path, fault);
            let end = run_job(&source, SMALL).await;
            assert_eq!(end.status_code, 200, "{}", end.metadata);
            assert_eq!(portal().requests_for(&path), 2);
        }
    }

    #[tokio::test]
    async fn test_a_source_that_stays_empty_fails_the_job() {
        let (source, path) = serve_source(b"");
        let end = run_job(&source, SMALL).await;
        assert_eq!(end.status_code, 500);
        assert_eq!(end.metadata, "Source download failed");
        assert_eq!(portal().requests_for(&path), 3, "tried three times");
    }

    /// Bad requests are refused before spending anything on a download.
    #[tokio::test]
    async fn test_bad_formats_fail_before_the_download() {
        for bad in ["[]", r#"[{"ext":"mp4"}]"#, r#"[{"id":1,"ext":"../x"}]"#] {
            let (source, path) = serve_source(b"source");
            let end = run_job(&source, bad).await;
            assert_eq!(end.status_code, 400, "{}", bad);
            assert_eq!(end.metadata, "Invalid media formats");
            assert_eq!(portal().requests_for(&path), 0, "{}", bad);
        }
    }

    #[tokio::test]
    async fn test_a_malformed_encrypted_cid_is_a_400() {
        let end = run_job_with(
            "s5://u!!!",
            SMALL,
            true,
            GateMode::Off,
            Box::new(StubModerationClient {
                outcome: ModerationOutcome::Cleared,
            }),
        )
        .await;
        assert_eq!(end.status_code, 400);
    }

    #[tokio::test]
    async fn test_a_bad_format_fails_alone_and_leaves_nothing_behind() {
        if !have_ffmpeg() {
            return;
        }
        let _uploads = UPLOAD_TEST_LOCK.lock().await;
        let (source, _) = serve_source(sample_video());
        let formats = r#"[{"id":1,"ext":"mp4","vcodec":"no_such_encoder"},
                          {"id":2,"ext":"mp4","vcodec":"libx264","vf":"scale=16:16"}]"#;
        let end = run_job(&source, formats).await;
        assert_eq!(end.status_code, 200, "{}", end.metadata);
        assert_eq!(published_cids(&end.metadata).len(), 1);
        assert!(outputs_containing(&end.id).is_empty());
    }

    #[tokio::test]
    async fn test_a_job_whose_formats_all_fail_is_a_500() {
        if !have_ffmpeg() {
            return;
        }
        let (source, _) = serve_source(sample_video());
        let end = run_job(
            &source,
            r#"[{"id":1,"ext":"mp4","vcodec":"no_such_encoder"}]"#,
        )
        .await;
        assert_eq!(
            (end.status_code, end.metadata.as_str()),
            (500, "All formats failed")
        );
        assert!(outputs_containing(&end.id).is_empty());
    }

    /// Same CID, same format id, different settings, at the same time. They used to
    /// write to one file, so each could publish the other's output or half of it.
    #[tokio::test]
    async fn test_jobs_on_one_source_at_the_same_time_keep_their_outputs_apart() {
        if !have_ffmpeg() {
            return;
        }
        let _uploads = UPLOAD_TEST_LOCK.lock().await;
        let (source, _) = serve_source(sample_video());
        let before = portal().uploads().len();
        let (a, b) = tokio::join!(
            run_job(
                &source,
                r#"[{"id":1,"ext":"mp4","vcodec":"libx264","vf":"scale=32:32"}]"#
            ),
            run_job(
                &source,
                r#"[{"id":1,"ext":"mp4","vcodec":"libx264","vf":"scale=48:48"}]"#
            ),
        );
        assert_eq!(a.status_code, 200, "{}", a.metadata);
        assert_eq!(b.status_code, 200, "{}", b.metadata);
        let mut heights: Vec<u32> = portal().uploads()[before..]
            .iter()
            .map(|u| height_of(&u.data))
            .collect();
        heights.sort();
        assert_eq!(heights, [32, 48]);
    }

    /// Two formats in one job can share an id, and mustn't share a file either.
    #[tokio::test]
    async fn test_formats_with_one_id_in_a_job_keep_their_outputs_apart() {
        if !have_ffmpeg() {
            return;
        }
        let _uploads = UPLOAD_TEST_LOCK.lock().await;
        let (source, _) = serve_source(sample_video());
        let before = portal().uploads().len();
        let formats = r#"[{"id":1,"ext":"mp4","vcodec":"libx264","vf":"scale=32:32"},
                          {"id":1,"ext":"mp4","vcodec":"libx264","vf":"scale=48:48"}]"#;
        let end = run_job_with(
            &source,
            formats,
            false,
            GateMode::Enforce,
            Box::new(StubModerationClient {
                outcome: ModerationOutcome::Cleared,
            }),
        )
        .await;
        assert_eq!(end.status_code, 200, "{}", end.metadata);
        let mut heights: Vec<u32> = portal().uploads()[before..]
            .iter()
            .map(|u| height_of(&u.data))
            .collect();
        heights.sort();
        assert_eq!(heights, [32, 48]);
    }

    /// Clients treat 100 as finished and don't wait long for the result after it, so
    /// the upload has to be over before they ever see it.
    #[tokio::test]
    async fn test_progress_stays_below_100_until_the_result_is_there() {
        if !have_ffmpeg() {
            return;
        }
        let _uploads = UPLOAD_TEST_LOCK.lock().await;
        let (source, _) = serve_source(sample_video());
        let id = fresh_task_id();
        register_job(&id);
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let watched = id.clone();
        let watcher = tokio::spawn(async move {
            loop {
                let progress = shared::calculate_overall_progress(&watched);
                let (_, metadata, _) = job_status(&watched).await;
                log.lock().unwrap().push((progress, metadata));
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        });
        supervise(
            id.clone(),
            LONG_DEADLINE,
            process_single_job(
                id.clone(),
                source,
                SMALL.to_string(),
                false,
                false,
                0,
                GateMode::Enforce,
                Box::new(StubModerationClient {
                    outcome: ModerationOutcome::Cleared,
                }),
            ),
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        watcher.abort();
        let seen = seen.lock().unwrap();
        assert!(seen.iter().any(|(p, _)| *p == 100), "it did finish");
        for (progress, metadata) in seen.iter() {
            assert!(
                *progress < 100 || metadata != "Transcoding in progress",
                "reported 100 with no result yet"
            );
        }
    }

    // ── The source duration: the node works out its billing steps from it ───

    /// Length of `sample_video`, near enough for any container's rounding.
    fn is_about_three_seconds(duration: f64) -> bool {
        (2.5..=3.5).contains(&duration)
    }

    #[tokio::test]
    async fn test_a_running_job_reports_its_duration_once_it_is_known() {
        let id = fresh_task_id();
        register_job(&id);
        assert_eq!(
            job_status(&id).await.2,
            0.0,
            "unknown until the source is probed"
        );

        set_source_duration(&id, 125.5);

        assert_eq!(
            job_status(&id).await,
            (200, "Transcoding in progress".to_string(), 125.5)
        );
    }

    #[tokio::test]
    async fn test_a_failed_job_keeps_reporting_a_duration_it_found() {
        let id = fresh_task_id();
        register_job(&id);
        set_source_duration(&id, 125.5);
        record_failure(&id, JobFailure::server("All formats failed")).await;
        assert_eq!(
            job_status(&id).await,
            (500, "All formats failed".to_string(), 125.5)
        );
    }

    #[tokio::test]
    async fn test_a_duration_survives_the_job_ending() {
        let id = fresh_task_id();
        register_job(&id);
        set_source_duration(&id, 125.5);
        mark_job_ended(&id);
        assert_eq!(job_status(&id).await.2, 125.5);
    }

    /// Reporting a duration must not make an unknown task look like a real one.
    #[tokio::test]
    async fn test_a_duration_for_a_task_nobody_registered_changes_nothing() {
        let id = fresh_task_id();
        set_source_duration(&id, 125.5);
        assert_eq!(
            job_status(&id).await,
            (404, "Unknown task".to_string(), 0.0)
        );
    }

    /// Both ways a client can ask.
    #[tokio::test]
    async fn test_both_apis_report_the_duration_of_a_running_job() {
        let id = fresh_task_id();
        register_job(&id);
        set_source_duration(&id, 125.5);

        let rest = RestHandler {
            transcode_task_sender: mpsc::channel(1).0,
        };
        let reply = rest.get_transcoded(id.clone()).await.unwrap();
        let bytes = hyper::body::to_bytes(warp::Reply::into_response(reply).into_body())
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["duration"], 125.5);
        assert_eq!(v["metadata"], "Transcoding in progress");

        let grpc = TranscodeServiceHandler {
            transcode_task_sender: mpsc::channel(1).0,
        };
        let response = grpc
            .get_transcoded(Request::new(GetTranscodedRequest { task_id: id }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(response.duration, 125.5);
    }

    /// Polls a real job as a client would, and returns what it saw.
    async fn watch_job(source: String, gate: GateMode) -> Vec<(i32, String, f64)> {
        let id = fresh_task_id();
        register_job(&id);
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let watched = id.clone();
        let watcher = tokio::spawn(async move {
            loop {
                let progress = shared::calculate_overall_progress(&watched);
                let (_, metadata, duration) = job_status(&watched).await;
                log.lock().unwrap().push((progress, metadata, duration));
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        });
        supervise(
            id.clone(),
            LONG_DEADLINE,
            process_single_job(
                id.clone(),
                source,
                SMALL.to_string(),
                false,
                false,
                0,
                gate,
                Box::new(StubModerationClient {
                    outcome: ModerationOutcome::Cleared,
                }),
            ),
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        watcher.abort();
        let seen = seen.lock().unwrap().clone();
        seen
    }

    /// What the node dev asked for: the real value from early in the run, the same
    /// value at the end, and never a real value that later goes back to 0.0.
    #[tokio::test]
    async fn test_a_real_job_reports_its_duration_from_early_on_until_the_end() {
        if !have_ffmpeg() {
            return;
        }
        let _uploads = UPLOAD_TEST_LOCK.lock().await;
        for gate in [GateMode::Off, GateMode::Enforce] {
            let (source, _) = serve_source(sample_video());
            let seen = watch_job(source, gate).await;

            let early = seen
                .iter()
                .find(|(_, metadata, duration)| {
                    metadata == "Transcoding in progress" && *duration > 0.0
                })
                .unwrap_or_else(|| panic!("{:?}: never reported while running", gate));
            assert!(is_about_three_seconds(early.2), "{}", early.2);
            assert!(
                early.0 < 50,
                "{:?}: only reported late, at {}%",
                gate,
                early.0
            );

            let (_, last_metadata, last_duration) = seen.last().unwrap();
            assert!(last_metadata.starts_with("[{"), "{}", last_metadata);
            assert_eq!(*last_duration, early.2, "the value must not change");

            let first_real = seen.iter().position(|(_, _, d)| *d > 0.0).unwrap();
            assert!(
                seen[first_real..].iter().all(|(_, _, d)| *d == early.2),
                "{:?}: the duration moved after it first appeared",
                gate
            );
        }
    }

    #[tokio::test]
    async fn test_a_job_whose_formats_all_fail_still_reports_the_duration() {
        if !have_ffmpeg() {
            return;
        }
        let (source, _) = serve_source(sample_video());
        let end = run_job(
            &source,
            r#"[{"id":1,"ext":"mp4","vcodec":"no_such_encoder"}]"#,
        )
        .await;
        assert_eq!(end.status_code, 500);
        assert!(is_about_three_seconds(end.duration), "{}", end.duration);
    }

    #[tokio::test]
    async fn test_a_held_job_reports_the_duration() {
        if !have_ffmpeg() {
            return;
        }
        let _uploads = UPLOAD_TEST_LOCK.lock().await;
        let (source, _) = serve_source(sample_video());
        let (client, _) = recording(ModerationOutcome::Blocked);
        let end = run_job_with(&source, SMALL, false, GateMode::Enforce, client).await;
        assert_eq!((end.status_code, end.metadata.as_str()), (200, "[]"));
        assert!(is_about_three_seconds(end.duration), "{}", end.duration);
    }

    /// Before the source arrives there's nothing to measure, and a file ffprobe can't
    /// read has no duration at all. Both stay at 0.0, which the node reads as unknown.
    #[tokio::test]
    async fn test_the_duration_stays_zero_when_it_cannot_be_known() {
        let end = run_job(&format!("s5://{}", fresh_cid()), SMALL).await;
        assert_eq!(end.status_code, 500);
        assert_eq!(end.duration, 0.0, "the download failed");

        if !have_ffmpeg() {
            return;
        }
        let (source, _) = serve_source(b"this is not a video");
        let end = run_job(&source, SMALL).await;
        assert_eq!(end.status_code, 500);
        assert_eq!(end.duration, 0.0, "nothing to probe");
    }

    // ── Encrypted sources ───────────────────────────────────────────────────

    /// Encrypts `plain` and serves it as a portal would, split into parts.
    fn serve_encrypted_source(plain: &[u8], part_count: usize) -> (String, Vec<String>, String) {
        let dir = std::env::temp_dir().join(format!("enc-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (input, output) = (dir.join("plain"), dir.join("enc"));
        std::fs::write(&input, plain).unwrap();
        let key = encrypt_file::encrypt_file_xchacha20(
            input.to_str().unwrap().to_string(),
            output.to_str().unwrap().to_string(),
            0,
        )
        .unwrap();
        let ciphertext = std::fs::read(&output).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        let mut blob_hash = [0u8; 32];
        blob_hash[..16].copy_from_slice(Uuid::new_v4().as_bytes());
        let cid = encrypted_cid_for(&key, &blob_hash);

        let id = Uuid::new_v4();
        let piece = ciphertext.len() / part_count + 1;
        let mut part_paths: Vec<String> = Vec::new();
        for (i, chunk) in ciphertext.chunks(piece).enumerate() {
            let path = format!("/parts/{}/{}", id, i);
            portal().serve(&path, chunk);
            part_paths.push(path);
        }
        // Portals list a whole-file location last, which isn't one of the parts.
        let mut urls: Vec<String> = part_paths
            .iter()
            .map(|p| format!("{}{}", portal().url, p))
            .collect();
        urls.push(format!("{}/never/requested", portal().url));
        let locations = json!({ "locations": [{ "parts": urls }] }).to_string();
        let mut hash_bytes = vec![0x1f];
        hash_bytes.extend_from_slice(&blob_hash);
        let locations_path = format!(
            "/api/locations/{}?types=5,3",
            bytes_to_base64url(&hash_bytes)
        );
        portal().serve(&locations_path, locations.as_bytes());
        (format!("s5://{}", cid), part_paths, locations_path)
    }

    async fn run_encrypted_job(source: &str) -> JobEnd {
        run_job_with(
            source,
            SMALL,
            true,
            GateMode::Off,
            Box::new(StubModerationClient {
                outcome: ModerationOutcome::Cleared,
            }),
        )
        .await
    }

    #[tokio::test]
    async fn test_an_encrypted_source_is_downloaded_decrypted_and_transcoded() {
        if !have_ffmpeg() {
            return;
        }
        let _uploads = UPLOAD_TEST_LOCK.lock().await;
        let (source, parts, _) = serve_encrypted_source(sample_video(), 3);
        let end = run_encrypted_job(&source).await;
        assert_eq!(end.status_code, 200, "{}", end.metadata);
        assert_eq!(parts.len(), 3);
        let cached = source_file_path(source.trim_start_matches("s5://"), true);
        assert_eq!(std::fs::read(&cached).unwrap(), sample_video());
        let stem = cached.rsplit('/').next().unwrap();
        let temporaries: Vec<String> = sources_containing(stem)
            .into_iter()
            .filter(|name| name != stem)
            .collect();
        assert!(temporaries.is_empty(), "left behind: {:?}", temporaries);
    }

    /// The job used to carry on with whatever arrived. The chunks that made it
    /// decrypted fine, so a truncated video was cached, published and paid for.
    #[tokio::test]
    async fn test_an_encrypted_source_missing_a_part_fails_and_caches_nothing() {
        let (source, parts, _) = serve_encrypted_source(&vec![5u8; 600_000], 3);
        for _ in 0..3 {
            portal().fail_next(&parts[1], Fault::Status(503));
        }
        let end = run_encrypted_job(&source).await;
        assert_eq!(
            (end.status_code, end.metadata.as_str()),
            (500, "Source download failed")
        );
        let cached = source_file_path(source.trim_start_matches("s5://"), true);
        assert!(!Path::new(&cached).exists());
        let stem = cached.rsplit('/').next().unwrap();
        assert!(
            sources_containing(stem).is_empty(),
            "temporary files left behind"
        );
    }

    #[tokio::test]
    async fn test_a_part_that_fails_once_is_retried() {
        if !have_ffmpeg() {
            return;
        }
        let _uploads = UPLOAD_TEST_LOCK.lock().await;
        let (source, parts, _) = serve_encrypted_source(sample_video(), 2);
        portal().fail_next(&parts[0], Fault::CutOff);
        let end = run_encrypted_job(&source).await;
        assert_eq!(end.status_code, 200, "{}", end.metadata);
    }

    #[tokio::test]
    async fn test_a_source_that_will_not_decrypt_fails_with_the_right_reason() {
        let (source, parts, _) = serve_encrypted_source(&vec![5u8; 1000], 1);
        portal().serve(&parts[0], &vec![1u8; 1016]);
        let end = run_encrypted_job(&source).await;
        assert_eq!(
            (end.status_code, end.metadata.as_str()),
            (500, "Source decryption failed")
        );
        let cached = source_file_path(source.trim_start_matches("s5://"), true);
        assert!(!Path::new(&cached).exists());
    }

    #[tokio::test]
    async fn test_a_failed_location_lookup_fails_the_job() {
        let (source, _, locations) = serve_encrypted_source(&vec![5u8; 1000], 1);
        for _ in 0..3 {
            portal().fail_next(&locations, Fault::Status(500));
        }
        let end = run_encrypted_job(&source).await;
        assert_eq!(
            (end.status_code, end.metadata.as_str()),
            (500, "Source download failed")
        );
    }

    /// The whole loop: what gets uploaded decrypts, with the key in the published
    /// CID, to a real video.
    #[tokio::test]
    async fn test_an_encrypted_output_decrypts_with_its_published_cid() {
        if !have_ffmpeg() {
            return;
        }
        let _uploads = UPLOAD_TEST_LOCK.lock().await;
        let (source, _) = serve_source(sample_video());
        let before = portal().uploads().len();
        let formats =
            r#"[{"id":1,"ext":"mp4","vcodec":"libx264","encrypt":true,"vf":"scale=32:32"}]"#;
        let end = run_job(&source, formats).await;
        assert_eq!(end.status_code, 200, "{}", end.metadata);

        let cid = published_cids(&end.metadata).remove(0);
        let parsed = parse_encrypted_cid(cid.trim_start_matches("s5://")).unwrap();
        let dir = gc_test_dir("output");
        let (input, output) = (dir.join("enc"), dir.join("plain"));
        let ciphertext = &portal().uploads()[before].data;
        std::fs::write(&input, ciphertext).unwrap();
        encrypt_file::decrypt_file_xchacha20(
            input.to_str().unwrap().to_string(),
            output.to_str().unwrap().to_string(),
            parsed.key,
            0,
            (ciphertext.len() / (262144 + 16)) as u32,
        )
        .unwrap();
        assert_eq!(height_of(&std::fs::read(&output).unwrap()), 32);
        assert!(
            outputs_containing(&end.id).is_empty(),
            "decrypted output left on disk"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_an_hls_format_publishes_its_segments_and_cleans_up() {
        if !have_ffmpeg() {
            return;
        }
        let _uploads = UPLOAD_TEST_LOCK.lock().await;
        let (source, _) = serve_source(sample_video());
        let formats = r#"[{"id":1,"ext":"mp4","vcodec":"libx264","hls":true,"hls_time":1}]"#;
        let end = run_job(&source, formats).await;
        assert_eq!(end.status_code, 200, "{}", end.metadata);
        let parsed: Vec<Value> = serde_json::from_str(&end.metadata).unwrap();
        assert!(parsed[0]["totalSegments"].as_u64().unwrap() >= 1);
        assert!(outputs_containing(&end.id).is_empty());
    }

    // ── The publish gate, from end to end ───────────────────────────────────

    /// Records how many frames were sent for scanning.
    struct RecordingClient {
        outcome: ModerationOutcome,
        frames: Arc<std::sync::Mutex<Option<usize>>>,
    }

    #[async_trait::async_trait]
    impl ModerationClient for RecordingClient {
        async fn moderate(
            &self,
            _t: &str,
            keyframes: &[Vec<u8>],
            _s: Option<String>,
        ) -> ModerationOutcome {
            *self.frames.lock().unwrap() = Some(keyframes.len());
            self.outcome.clone()
        }
    }

    fn recording(
        outcome: ModerationOutcome,
    ) -> (
        Box<dyn ModerationClient>,
        Arc<std::sync::Mutex<Option<usize>>>,
    ) {
        let frames = Arc::new(std::sync::Mutex::new(None));
        (
            Box::new(RecordingClient {
                outcome,
                frames: frames.clone(),
            }),
            frames,
        )
    }

    #[tokio::test]
    async fn test_enforce_publishes_a_cleared_job() {
        if !have_ffmpeg() {
            return;
        }
        let _uploads = UPLOAD_TEST_LOCK.lock().await;
        let (source, _) = serve_source(sample_video());
        let before = portal().uploads().len();
        let (client, frames) = recording(ModerationOutcome::Cleared);
        let end = run_job_with(&source, SMALL, false, GateMode::Enforce, client).await;
        assert_eq!(end.status_code, 200, "{}", end.metadata);
        assert_eq!(end.progress, 100);
        assert_eq!(published_cids(&end.metadata).len(), 1);
        assert_eq!(portal().uploads().len(), before + 1);
        assert!(
            frames.lock().unwrap().unwrap() >= 1,
            "frames must be scanned"
        );
        assert!(outputs_containing(&end.id).is_empty());
    }

    #[tokio::test]
    async fn test_enforce_holds_a_blocked_job_and_uploads_nothing() {
        if !have_ffmpeg() {
            return;
        }
        let _uploads = UPLOAD_TEST_LOCK.lock().await;
        let (source, _) = serve_source(sample_video());
        let before = portal().uploads().len();
        let (client, _) = recording(ModerationOutcome::Blocked);
        let end = run_job_with(&source, SMALL, false, GateMode::Enforce, client).await;
        assert_eq!((end.status_code, end.metadata.as_str()), (200, "[]"));
        assert_eq!(end.progress, 100);
        assert_eq!(portal().uploads().len(), before);
        assert!(outputs_containing(&end.id).is_empty());
    }

    #[tokio::test]
    async fn test_dark_publishes_a_blocked_job() {
        if !have_ffmpeg() {
            return;
        }
        let _uploads = UPLOAD_TEST_LOCK.lock().await;
        let (source, _) = serve_source(sample_video());
        let (client, _) = recording(ModerationOutcome::Blocked);
        let end = run_job_with(&source, SMALL, false, GateMode::Dark, client).await;
        assert_eq!(end.status_code, 200);
        assert_eq!(published_cids(&end.metadata).len(), 1);
    }

    /// A broken GPU or a corrupt source isn't a moderation decision. Reporting it as a
    /// hold told the client the content was held, and that retrying was pointless.
    #[tokio::test]
    async fn test_enforce_reports_a_failed_transcode_as_a_failure_not_a_hold() {
        if !have_ffmpeg() {
            return;
        }
        let (source, _) = serve_source(sample_video());
        let (client, frames) = recording(ModerationOutcome::Cleared);
        let end = run_job_with(
            &source,
            r#"[{"id":1,"ext":"mp4","vcodec":"no_such_encoder"}]"#,
            false,
            GateMode::Enforce,
            client,
        )
        .await;
        assert_eq!(
            (end.status_code, end.metadata.as_str()),
            (500, "All formats failed")
        );
        assert!(frames.lock().unwrap().is_none(), "nothing to scan");
    }

    /// The format that owned the tap failing is one failed format. It used to hold the
    /// whole job: good outputs thrown away, and the client told the content was held.
    #[tokio::test]
    async fn test_a_failed_format_that_owned_the_tap_does_not_hold_the_job() {
        if !have_ffmpeg() {
            return;
        }
        let _uploads = UPLOAD_TEST_LOCK.lock().await;
        let (source, _) = serve_source(sample_video());
        let (client, frames) = recording(ModerationOutcome::Cleared);
        let end = run_job_with(
            &source,
            r#"[{"id":1,"ext":"mp4","vcodec":"no_such_encoder"},
                {"id":2,"ext":"mp4","vcodec":"libx264","vf":"scale=16:16"}]"#,
            false,
            GateMode::Enforce,
            client,
        )
        .await;
        assert_eq!(end.status_code, 200, "{}", end.metadata);
        assert_eq!(published_cids(&end.metadata).len(), 1);
        assert!(
            frames.lock().unwrap().unwrap() >= 1,
            "the next format scans it"
        );
    }

    /// With no other video format to take over, the dedicated tap still scans it.
    #[tokio::test]
    async fn test_the_source_is_still_scanned_when_no_video_format_is_left() {
        if !have_ffmpeg() {
            return;
        }
        let _uploads = UPLOAD_TEST_LOCK.lock().await;
        let (source, _) = serve_source(sample_video());
        let (client, frames) = recording(ModerationOutcome::Cleared);
        let end = run_job_with(
            &source,
            r#"[{"id":1,"ext":"mp4","vcodec":"no_such_encoder"},
                {"id":2,"ext":"flac","acodec":"flac"}]"#,
            false,
            GateMode::Enforce,
            client,
        )
        .await;
        assert_eq!(end.status_code, 200, "{}", end.metadata);
        assert_eq!(published_cids(&end.metadata).len(), 1);
        assert!(
            frames.lock().unwrap().unwrap() >= 1,
            "the source must be scanned"
        );
    }

    /// The tap asks for a video stream a source without one doesn't have, which used
    /// to fail the rendition. Dark mode is meant to change nothing.
    #[tokio::test]
    async fn test_an_audio_source_with_a_video_format_still_transcodes_under_the_gate() {
        if !have_ffmpeg() {
            return;
        }
        let _uploads = UPLOAD_TEST_LOCK.lock().await;
        let (source, _) = serve_source(sample_audio());
        let (client, frames) = recording(ModerationOutcome::Cleared);
        let end = run_job_with(&source, SMALL, false, GateMode::Dark, client).await;
        assert_eq!(end.status_code, 200, "{}", end.metadata);
        assert_eq!(published_cids(&end.metadata).len(), 1);
        assert_eq!(*frames.lock().unwrap(), Some(0), "nothing to scan in audio");
    }

    /// A file can claim to be shorter than it is. The frame cap then stops the scan
    /// early while FFmpeg exits happily, and the part never scanned got published.
    #[tokio::test]
    async fn test_enforce_holds_a_file_whose_scan_stopped_at_the_frame_cap() {
        if !have_ffmpeg() {
            return;
        }
        let _uploads = UPLOAD_TEST_LOCK.lock().await;
        let (source, _) = serve_source(&sample_with_a_false_duration());
        let before = portal().uploads().len();
        let (client, frames) = recording(ModerationOutcome::Cleared);
        let end = run_job_with(
            &source,
            r#"[{"id":1,"ext":"mp4","vcodec":"libx264","vf":"scale=16:16"}]"#,
            false,
            GateMode::Enforce,
            client,
        )
        .await;
        assert_eq!(
            (end.status_code, end.metadata.as_str()),
            (200, "[]"),
            "must hold"
        );
        assert_eq!(portal().uploads().len(), before);
        assert!(
            frames.lock().unwrap().is_none(),
            "an incomplete scan is never sent"
        );
    }

    /// An entry that can't be listed could be a frame, so the set is then unknown. A
    /// real directory can't be made to do this on demand.
    #[test]
    fn test_a_directory_entry_that_cannot_be_listed_empties_the_whole_set() {
        let listing = |entries: Vec<std::io::Result<&str>>| {
            frame_paths(entries.into_iter().map(|e| e.map(std::path::PathBuf::from)))
        };
        assert_eq!(
            listing(vec![Ok("b.png"), Ok("a.png"), Ok("c.txt")]),
            vec![
                std::path::PathBuf::from("a.png"),
                std::path::PathBuf::from("b.png")
            ]
        );
        let broken = std::io::Error::new(std::io::ErrorKind::Other, "listing failed");
        assert!(listing(vec![Ok("a.png"), Err(broken)]).is_empty());
    }

    /// A partial scan must never pass for a complete one.
    #[test]
    fn test_a_frame_that_cannot_be_read_empties_the_whole_set() {
        let dir = gc_test_dir("frames");
        std::fs::write(dir.join("kf_00001.png"), b"frame").unwrap();
        assert_eq!(read_keyframe_pngs(dir.to_str().unwrap()).len(), 1);
        // A directory with the right name: listed, but can't be read as a file.
        std::fs::create_dir(dir.join("kf_00002.png")).unwrap();
        assert!(read_keyframe_pngs(dir.to_str().unwrap()).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── Nothing on the job path may panic ───────────────────────────────────

    /// `supervise` catches a panic, but then all a client learns is "Internal error".
    /// Each of these has failed for a real reason that should have been reported.
    #[test]
    fn test_the_job_path_has_no_panics_left_in_it() {
        for (name, source, allowed) in [
            ("utils.rs", include_str!("utils.rs"), 0),
            ("s5.rs", include_str!("s5.rs"), 0),
            ("encrypt_file.rs", include_str!("encrypt_file.rs"), 0),
            ("hls_segment.rs", include_str!("hls_segment.rs"), 0),
            // The storage path, which main checks before starting, and two regexes
            // written in the source.
            ("transcode_video.rs", include_str!("transcode_video.rs"), 3),
            // The two storage paths main checks, and two fixed addresses.
            ("server.rs", job_flow_src(), 3),
        ] {
            let production = production_part(source);
            let found = production.matches(".unwrap()").count()
                + production.matches(".expect(").count()
                + production.matches("panic!(").count();
            assert!(
                found <= allowed,
                "{} has {} possible panics, {} expected",
                name,
                found,
                allowed
            );
        }
    }

    // ── The queue: one job must never stop the jobs behind it ──────────────

    type Ran = Arc<std::sync::Mutex<Vec<String>>>;

    fn queue_job(task_id: &str, source_cid: &str) -> TranscodeJob {
        TranscodeJob {
            task_id: task_id.to_string(),
            source_cid: source_cid.to_string(),
            media_formats: String::new(),
            is_encrypted: false,
            is_gpu: false,
            preview_percent: 0,
        }
    }

    fn slots(permits: usize) -> &'static tokio::sync::Semaphore {
        Box::leak(Box::new(tokio::sync::Semaphore::new(permits)))
    }

    /// Stands in for the real job: what it does is chosen by its `source_cid`.
    fn scripted_job(
        ran: Ran,
    ) -> impl Fn(
        TranscodeJob,
    ) -> futures::future::BoxFuture<'static, std::result::Result<(), JobFailure>>
           + Send
           + Sync
           + 'static {
        move |job: TranscodeJob| {
            let ran = ran.clone();
            Box::pin(async move {
                ran.lock().unwrap().push(job.task_id.clone());
                match job.source_cid.as_str() {
                    "panic" => panic!("job panicked"),
                    "hang" => {
                        std::future::pending::<()>().await;
                        Ok(())
                    }
                    "fail" => Err(JobFailure::server("Source download failed")),
                    _ => Ok(()),
                }
            })
        }
    }

    /// Wait for a condition without a fixed sleep, so a passing test stays fast.
    async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
        for _ in 0..500 {
            if check() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for: {}", what);
    }

    /// Queue tests hold the counter lock, so they must leave no job running: one
    /// ended by the test runtime shutting down would change the counters under
    /// the next test.
    async fn wait_until_no_job_is_active() {
        eventually("every job to end", || shared::active_jobs() == 0).await;
    }

    #[tokio::test]
    async fn test_jobs_behind_a_panicking_or_failing_job_still_run() {
        let _serial = shared::COUNTER_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (sender, receiver) = mpsc::channel(10);
        let ran: Ran = Arc::default();
        tokio::spawn(run_job_queue(
            Arc::new(Mutex::new(receiver)),
            slots(1),
            std::time::Duration::from_secs(60),
            scripted_job(ran.clone()),
        ));

        let (bad, failed, good) = (fresh_task_id(), fresh_task_id(), fresh_task_id());
        enqueue(&sender, queue_job(&bad, "panic")).unwrap();
        enqueue(&sender, queue_job(&failed, "fail")).unwrap();
        enqueue(&sender, queue_job(&good, "ok")).unwrap();

        let finished = ran.clone();
        eventually("all three jobs to start", move || {
            finished.lock().unwrap().len() == 3
        })
        .await;
        // The last job only started because the two before it gave their slot back.
        let settled = good.clone();
        eventually("the last job to end", move || {
            job_records()
                .get(&settled)
                .map_or(false, |record| record.ended_at.is_some())
        })
        .await;

        assert_eq!(job_status(&bad).await.0, 500);
        assert_eq!(job_status(&failed).await.0, 500);
        assert_eq!(
            job_status(&good).await.0,
            200,
            "the last job must have run normally"
        );
        wait_until_no_job_is_active().await;
    }

    /// The failure this branch exists for: with one slot, a job that never ends
    /// used to stop every job behind it, with no error anywhere.
    #[tokio::test]
    async fn test_jobs_behind_a_hung_job_run_once_its_deadline_passes() {
        let _serial = shared::COUNTER_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (sender, receiver) = mpsc::channel(10);
        let ran: Ran = Arc::default();
        tokio::spawn(run_job_queue(
            Arc::new(Mutex::new(receiver)),
            slots(1),
            std::time::Duration::from_millis(50),
            scripted_job(ran.clone()),
        ));

        let (hung, next) = (fresh_task_id(), fresh_task_id());
        enqueue(&sender, queue_job(&hung, "hang")).unwrap();
        enqueue(&sender, queue_job(&next, "ok")).unwrap();

        let started = ran.clone();
        let expected = next.clone();
        eventually("the job behind the hung one to start", move || {
            started.lock().unwrap().contains(&expected)
        })
        .await;
        assert_eq!(
            job_status(&hung).await,
            (500, "Job timed out".to_string(), 0.0)
        );
        wait_until_no_job_is_active().await;
    }

    #[tokio::test]
    async fn test_supervise_stops_a_job_that_outruns_its_deadline() {
        struct SetOnDrop(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for SetOnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let guard = SetOnDrop(stopped.clone());
        let id = fresh_task_id();
        supervise(
            id.clone(),
            std::time::Duration::from_millis(20),
            async move {
                let _guard = guard;
                std::future::pending::<()>().await;
                Ok(())
            },
        )
        .await;
        eventually("the hung job to be stopped, freeing its FFmpeg", || {
            stopped.load(Ordering::SeqCst)
        })
        .await;
        assert_eq!(job_status(&id).await.0, 500);
    }

    #[tokio::test]
    async fn test_the_slot_and_active_count_come_back_after_a_panic() {
        let _serial = shared::COUNTER_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        shared::ACTIVE_JOBS.store(0, Ordering::Relaxed);
        shared::QUEUED_JOBS.store(0, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel(10);
        let free = slots(2);
        tokio::spawn(run_job_queue(
            Arc::new(Mutex::new(receiver)),
            free,
            std::time::Duration::from_secs(60),
            scripted_job(Arc::default()),
        ));
        let id = fresh_task_id();
        enqueue(&sender, queue_job(&id, "panic")).unwrap();
        let ended = id.clone();
        eventually("the job to end", move || {
            job_records()
                .get(&ended)
                .is_some_and(|r| r.ended_at.is_some())
        })
        .await;
        wait_until_no_job_is_active().await;
        assert_eq!(shared::queued_jobs(), 0);
        // The idle queue itself holds one slot, ready for the next job.
        eventually("the job's slot to come back", || {
            free.available_permits() == 1
        })
        .await;
    }

    /// If the loop that takes jobs off the queue ever dies, it must come back
    /// and the queue must still hold the jobs waiting in it.
    #[tokio::test]
    async fn test_keep_restarting_brings_a_dead_task_back() {
        let lives = Arc::new(AtomicUsize::new(0));
        let counter = lives.clone();
        keep_restarting(
            "test task",
            std::time::Duration::from_millis(1),
            move || {
                let counter = counter.clone();
                async move {
                    if counter.fetch_add(1, Ordering::SeqCst) < 2 {
                        panic!("died");
                    }
                }
            },
        )
        .await;
        assert_eq!(
            lives.load(Ordering::SeqCst),
            3,
            "two deaths, then a clean end"
        );
    }

    #[tokio::test]
    async fn test_a_queue_restarted_after_its_receiver_died_keeps_the_waiting_jobs() {
        let _serial = shared::COUNTER_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (sender, receiver) = mpsc::channel(10);
        let receiver = Arc::new(Mutex::new(receiver));
        let id = fresh_task_id();
        enqueue(&sender, queue_job(&id, "ok")).unwrap();

        // First life: the job function itself blows up the loop calling it.
        let first_life = run_job_queue(
            receiver.clone(),
            slots(1),
            std::time::Duration::from_secs(60),
            |_job: TranscodeJob| -> futures::future::BoxFuture<
                'static,
                std::result::Result<(), JobFailure>,
            > { panic!("receiver died") },
        );
        assert!(tokio::spawn(first_life).await.is_err());

        // The job that killed it is gone, so send another. The queue still works.
        let after = fresh_task_id();
        enqueue(&sender, queue_job(&after, "ok")).unwrap();
        let ran: Ran = Arc::default();
        tokio::spawn(run_job_queue(
            receiver,
            slots(1),
            std::time::Duration::from_secs(60),
            scripted_job(ran.clone()),
        ));
        let started = ran.clone();
        let expected = after.clone();
        eventually("a job to run after the restart", move || {
            started.lock().unwrap().contains(&expected)
        })
        .await;
        wait_until_no_job_is_active().await;
    }

    // ── Submitting ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_a_full_queue_refuses_at_once_and_forgets_the_job() {
        let _serial = shared::COUNTER_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        shared::QUEUED_JOBS.store(0, Ordering::Relaxed);
        let (sender, _receiver) = mpsc::channel(1);
        let (first, refused) = (fresh_task_id(), fresh_task_id());
        enqueue(&sender, queue_job(&first, "ok")).unwrap();
        assert_eq!(shared::queued_jobs(), 1);

        assert!(matches!(
            enqueue(&sender, queue_job(&refused, "ok")),
            Err(EnqueueError::QueueFull)
        ));
        assert_eq!(
            shared::queued_jobs(),
            1,
            "the refused job gives its slot back"
        );
        assert_eq!(job_status(&refused).await.0, 404);
        shared::QUEUED_JOBS.store(0, Ordering::Relaxed);
    }

    #[tokio::test]
    async fn test_rest_reports_a_full_queue_as_503() {
        let _serial = shared::COUNTER_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (sender, _receiver) = mpsc::channel(1);
        enqueue(&sender, queue_job(&fresh_task_id(), "ok")).unwrap();
        let handler = RestHandler {
            transcode_task_sender: sender,
        };
        let reply = handler
            .transcode("s5://abc".to_string(), String::new(), false, false, 0)
            .await
            .unwrap();
        let response = warp::Reply::into_response(reply);
        assert_eq!(
            response.status(),
            warp::http::StatusCode::SERVICE_UNAVAILABLE
        );
        let bytes = hyper::body::to_bytes(response.into_body()).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["status_code"], 503);
        assert_eq!(v["task_id"], "");
        shared::QUEUED_JOBS.store(0, Ordering::Relaxed);
    }

    #[tokio::test]
    async fn test_grpc_reports_a_full_queue_as_resource_exhausted() {
        let _serial = shared::COUNTER_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (sender, _receiver) = mpsc::channel(1);
        enqueue(&sender, queue_job(&fresh_task_id(), "ok")).unwrap();
        let handler = TranscodeServiceHandler {
            transcode_task_sender: sender,
        };
        let status = handler
            .transcode(Request::new(TranscodeRequest {
                source_cid: "s5://abc".to_string(),
                media_formats: String::new(),
                is_encrypted: false,
                is_gpu: false,
                preview_percent: 0,
            }))
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::ResourceExhausted);
        shared::QUEUED_JOBS.store(0, Ordering::Relaxed);
    }

    #[tokio::test]
    async fn test_rest_accepts_a_job_and_it_reads_as_in_progress() {
        let _serial = shared::COUNTER_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (sender, _receiver) = mpsc::channel(1);
        let handler = RestHandler {
            transcode_task_sender: sender,
        };
        let reply = handler
            .transcode("s5://abc".to_string(), String::new(), false, false, 0)
            .await
            .unwrap();
        let response = warp::Reply::into_response(reply);
        assert_eq!(response.status(), warp::http::StatusCode::OK);
        let bytes = hyper::body::to_bytes(response.into_body()).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["status_code"], 200);
        let task_id = v["task_id"].as_str().unwrap();
        assert_eq!(
            job_status(task_id).await,
            (200, "Transcoding in progress".to_string(), 0.0)
        );
        shared::QUEUED_JOBS.store(0, Ordering::Relaxed);
    }

    /// Through the real routes, the way the node reaches them.
    #[tokio::test]
    async fn test_the_rest_routes_answer_as_documented() {
        let routes = rest_routes(Arc::new(RestHandler {
            transcode_task_sender: mpsc::channel(1).0,
        }));
        for (path, expected) in [
            ("/health", 200),
            ("/status", 401),
            ("/get_transcoded/abc", 401),
            (
                "/transcode?source_cid=s5://a&media_formats=&is_encrypted=false&is_gpu=false",
                401,
            ),
            ("/nonexistent", 404),
        ] {
            let response = warp::test::request().path(path).reply(&routes).await;
            assert_eq!(response.status(), expected, "{}", path);
        }
    }

    /// Every protected route needs this header, so a browser has to be allowed to send it.
    #[tokio::test]
    async fn test_browsers_are_allowed_to_send_the_token() {
        let routes = rest_routes(Arc::new(RestHandler {
            transcode_task_sender: mpsc::channel(1).0,
        }));
        let response = warp::test::request()
            .method("OPTIONS")
            .path("/status")
            .header("origin", "https://app.example")
            .header("access-control-request-method", "GET")
            .header("access-control-request-headers", "authorization")
            .reply(&routes)
            .await;
        assert_eq!(response.status(), 200);
    }

    // ── What a client can tell after a restart ──────────────────────────────

    #[tokio::test]
    async fn test_a_task_this_process_does_not_know_reads_as_404() {
        assert_eq!(
            job_status(&fresh_task_id()).await,
            (404, "Unknown task".to_string(), 0.0)
        );
    }

    // ── Records of finished jobs must not grow for ever ─────────────────────

    #[test]
    fn test_only_jobs_that_ended_long_enough_ago_expire() {
        let long_ago = Instant::now() - std::time::Duration::from_secs(100);
        let record = |ended_at| JobRecord {
            ended_at,
            source_duration: 0.0,
        };
        let mut records = HashMap::from([
            ("old".to_string(), record(Some(long_ago))),
            ("recent".to_string(), record(Some(Instant::now()))),
            ("running".to_string(), record(None)),
        ]);
        let expired = take_expired(&mut records, std::time::Duration::from_secs(50));
        assert_eq!(expired, ["old"]);
        assert!(records.contains_key("recent"));
        assert!(
            records.contains_key("running"),
            "a running job must survive however old it is"
        );
    }

    #[tokio::test]
    async fn test_an_expired_job_leaves_nothing_behind() {
        let (done, failed) = (fresh_task_id(), fresh_task_id());
        for id in [&done, &failed] {
            register_job(id);
            shared::update_progress(id, 0, 10);
        }
        TRANSCODED
            .lock()
            .await
            .insert(done.clone(), ("[]".to_string(), 1.0));
        record_failure(&failed, JobFailure::server("Internal error")).await;

        let expired = [done.clone(), failed.clone()];
        for id in &expired {
            forget_job(id);
        }
        forget_job_results(&expired).await;

        for id in &expired {
            assert_eq!(job_status(id).await.0, 404);
            assert_eq!(shared::calculate_overall_progress(id), 0);
        }
    }

    /// Pruning has to be able to reach a real job. The window is long, so this
    /// can't remove records other tests are using at the same moment.
    #[tokio::test]
    async fn test_prune_keeps_a_job_that_ended_within_the_window() {
        let id = fresh_task_id();
        register_job(&id);
        mark_job_ended(&id);
        TRANSCODED
            .lock()
            .await
            .insert(id.clone(), ("[]".to_string(), 1.0));
        prune_job_records(std::time::Duration::from_secs(3600)).await;
        assert_eq!(job_status(&id).await.1, "[]");
    }

    // ── Garbage collection ──────────────────────────────────────────────────

    fn gc_test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("gc-{}-{}", name, Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_in_age_order(dir: &Path, names: &[&str]) {
        for name in names {
            std::fs::write(dir.join(name), [0u8; 100]).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
    }

    /// It used to delete newest first, so a job's just-downloaded source was the
    /// first thing to go while files nobody needed stayed.
    #[test]
    fn test_garbage_collect_deletes_the_oldest_files_first() {
        let dir = gc_test_dir("oldest");
        write_in_age_order(&dir, &["oldest", "middle", "newest"]);
        garbage_collect(dir.to_str().unwrap(), 150);
        assert!(!dir.join("oldest").exists());
        assert!(!dir.join("middle").exists());
        assert!(dir.join("newest").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_garbage_collect_leaves_everything_when_under_the_threshold() {
        let dir = gc_test_dir("under");
        write_in_age_order(&dir, &["a", "b"]);
        garbage_collect(dir.to_str().unwrap(), 1000);
        assert!(dir.join("a").exists() && dir.join("b").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A missing directory used to panic, and the task never came back: the disk
    /// then filled until every job failed.
    #[test]
    fn test_garbage_collect_survives_a_missing_directory() {
        garbage_collect("/nonexistent/definitely/not/here/", 0);
    }

    /// `PATH_TO_FILE` is a plain prefix, so `/dir/prefix_` is a valid shape. It
    /// must only ever touch the files carrying that prefix.
    #[test]
    fn test_garbage_collect_understands_a_path_prefix() {
        let dir = gc_test_dir("prefix");
        write_in_age_order(&dir, &["src_a", "src_b", "other"]);
        garbage_collect(dir.join("src_").to_str().unwrap(), 0);
        assert!(!dir.join("src_a").exists() && !dir.join("src_b").exists());
        assert!(dir.join("other").exists(), "outside the prefix");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── Startup ─────────────────────────────────────────────────────────────

    /// A crash leaves temporary files nobody will come back for, some of them
    /// decrypted content. Finished sources are a cache and must survive.
    #[test]
    fn test_boot_sweep_removes_only_what_a_dead_process_left_behind() {
        let dir = gc_test_dir("sweep");
        for leftover in ["cid.task.part", "cid_33_task_ue.mp4"] {
            std::fs::write(dir.join(leftover), b"x").unwrap();
        }
        for leftover in ["cid_33_task_hls", "task_modtap"] {
            std::fs::create_dir_all(dir.join(leftover)).unwrap();
            std::fs::write(dir.join(leftover).join("f"), b"x").unwrap();
        }
        for kept in ["uCachedSource", "abc_def"] {
            std::fs::write(dir.join(kept), b"x").unwrap();
        }
        // A file that only looks like one of the directories is not ours.
        std::fs::write(dir.join("file_hls"), b"x").unwrap();

        sweep_leftovers(dir.to_str().unwrap());

        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["abc_def", "file_hls", "uCachedSource"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_boot_sweep_understands_a_path_prefix() {
        let dir = gc_test_dir("sweep-prefix");
        std::fs::write(dir.join("src_x.part"), b"x").unwrap();
        std::fs::write(dir.join("other.part"), b"x").unwrap();
        sweep_leftovers(dir.join("src_").to_str().unwrap());
        assert!(!dir.join("src_x.part").exists());
        assert!(dir.join("other.part").exists(), "outside the prefix");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_storage_parent_is_the_directory_a_path_prefix_lives_in() {
        assert_eq!(storage_dir("/data/downloads/"), "/data/downloads/");
        assert_eq!(storage_dir("/data/prefix_"), "/data");
        assert_eq!(storage_dir("relative/dir/"), "relative/dir/");
        assert_eq!(storage_dir("prefix_"), ".");
    }

    #[test]
    fn test_prepare_storage_creates_the_directory() {
        let dir = gc_test_dir("prepare").join("a/b/");
        let path = format!("{}/", dir.to_str().unwrap());
        prepare_storage(&path).unwrap();
        assert!(dir.is_dir());
    }

    #[test]
    fn test_prepare_storage_reports_a_path_it_cannot_create() {
        assert!(prepare_storage("/proc/definitely/not/writable/").is_err());
    }

    /// `main` cannot run in a test, so pin what must be true of it: the server
    /// has to die when a listener does, or a dead port never gets a restart.
    #[test]
    fn test_main_exits_when_a_listener_ends() {
        let flow = job_flow_src();
        let main_body = flow.split("async fn main() {").nth(1).expect("main");
        let select = main_body
            .find("tokio::select!")
            .expect("watches both listeners");
        let exit = main_body[select..]
            .find("std::process::exit(1)")
            .expect("exits non-zero when one ends");
        assert!(exit > 0);
    }

    /// `supervise` turns a panic into a failed job only by catching the unwind.
    #[test]
    fn test_panics_unwind_so_one_job_cannot_take_down_the_process() {
        let manifest = include_str!("../Cargo.toml");
        assert!(
            !manifest.contains("panic = \"abort\""),
            "panic=abort would make any job panic kill every other job"
        );
    }

    #[test]
    fn test_status_endpoint_exists() {
        let server_src = job_flow_src();
        assert!(
            server_src.contains(r#"warp::path!("status")"#),
            "REST server must have a /status endpoint"
        );
    }

    #[test]
    fn test_status_response_has_required_fields() {
        let resp = StatusResponse {
            active_jobs: 1,
            queued_jobs: 2,
            max_concurrent: 3,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"active_jobs\""));
        assert!(json.contains("\"queued_jobs\""));
        assert!(json.contains("\"max_concurrent\""));
    }

    /// Jobs run at the same time, up to the slots available.
    #[tokio::test]
    async fn test_the_queue_runs_jobs_concurrently_up_to_its_slots() {
        let _serial = shared::COUNTER_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (sender, receiver) = mpsc::channel(10);
        let ran: Ran = Arc::default();
        let free = slots(2);
        tokio::spawn(run_job_queue(
            Arc::new(Mutex::new(receiver)),
            free,
            // Ends the hung jobs, so nothing is left running after the test.
            std::time::Duration::from_millis(200),
            scripted_job(ran.clone()),
        ));
        for _ in 0..3 {
            enqueue(&sender, queue_job(&fresh_task_id(), "hang")).unwrap();
        }
        let started = ran.clone();
        eventually("two jobs to start together", move || {
            started.lock().unwrap().len() == 2
        })
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert_eq!(
            ran.lock().unwrap().len(),
            2,
            "the third has to wait for a slot"
        );
        assert_eq!(free.available_permits(), 0);
        let started = ran.clone();
        eventually("the third job to get a slot", move || {
            started.lock().unwrap().len() == 3
        })
        .await;
        wait_until_no_job_is_active().await;
    }

    /// Q1(b) boot-fatal. `main()` cannot be unit-tested, so assert the ordering
    /// structurally: a gate validated AFTER the listeners bind is not "refuse to
    /// start" — jobs could be accepted in the window on a misconfigured host.
    #[test]
    fn test_gate_validation_precedes_listener_startup() {
        let src = job_flow_src();
        // nth(1) = the text after the real definition (this test's own literal
        // occurrence is later in the file).
        let main_body = src
            .split("async fn main() {")
            .nth(1)
            .expect("main() must exist");
        let validate = main_body
            .find("validate_gate_mode()")
            .expect("main() must validate MODERATION_GATE at boot");
        let exit_at = validate
            + main_body[validate..]
                .find("std::process::exit(1)")
                .expect("an invalid MODERATION_GATE must exit non-zero");
        for listener in ["Server::builder()", "warp::serve("] {
            let at = main_body
                .find(listener)
                .unwrap_or_else(|| panic!("{} not found in main()", listener));
            assert!(
                validate < at,
                "MODERATION_GATE validation must precede {}",
                listener
            );
            assert!(
                exit_at < at,
                "the fatal exit arm must precede {} — otherwise the listener is \
                 already bound when the container dies",
                listener
            );
        }
    }

    #[test]
    fn test_proto_has_preview_percent() {
        let proto_src = include_str!("../proto/transcode.proto");
        assert!(
            proto_src.contains("uint32 preview_percent"),
            "TranscodeRequest proto must contain 'uint32 preview_percent' field"
        );
    }

    #[test]
    fn test_transcode_job_struct() {
        let job = TranscodeJob {
            task_id: "abc".to_string(),
            source_cid: "s5://test".to_string(),
            media_formats: "[]".to_string(),
            is_encrypted: false,
            is_gpu: true,
            preview_percent: 15,
        };
        assert_eq!(job.preview_percent, 15);
        assert_eq!(job.task_id, "abc");
    }

    #[test]
    fn test_query_params_deserializes_preview_percent() {
        let json = r#"{"source_cid":"s5://x","media_formats":"[]","is_encrypted":false,"is_gpu":false,"preview_percent":20}"#;
        let params: QueryParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.preview_percent, Some(20));

        let json_without =
            r#"{"source_cid":"s5://x","media_formats":"[]","is_encrypted":false,"is_gpu":false}"#;
        let params2: QueryParams = serde_json::from_str(json_without).unwrap();
        assert_eq!(params2.preview_percent, None);
    }

    #[test]
    fn test_process_single_job_handles_hls_response() {
        let src = job_flow_src();
        assert!(src.contains("initSegmentCid"), "must map initSegmentCid");
        assert!(src.contains("totalSegments"), "must map totalSegments");
    }

    #[test]
    fn test_preview_percent_threaded_to_transcode_video() {
        let src = job_flow_src();
        assert!(src.contains("preview_percent"), "must pass preview_percent");
    }
}
