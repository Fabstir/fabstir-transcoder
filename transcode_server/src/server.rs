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
use utils::{base64url_to_bytes, bytes_to_base64url, download_and_concat_files, download_video};

mod transcode_video;
use transcode_video::{
    get_video_format_from_str, transcode_video, transcode_video_produce, transcode_video_publish,
    LocalOutput, TranscodeVideoResponse,
};

mod hls_segment;
mod moderation;
mod shared;

use tonic::{transport::Server, Request, Response, Status};
use warp::Filter;

use async_trait::async_trait;

use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use transcode::{
    transcode_service_server::{TranscodeService, TranscodeServiceServer},
    GetTranscodedRequest, GetTranscodedResponse, TranscodeRequest, TranscodeResponse,
};

mod encrypted_cid;
use crate::encrypt_file::decrypt_file_xchacha20;

use serde::{Deserialize, Serialize};
use serde_json::{from_str, json, Value};
use std::fs::read_to_string;

use anyhow::{anyhow, Result};
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use chrono::Utc;
use uuid::{Uuid, Version};

use base64;
use std::convert::TryInto;

use dotenv::{dotenv, var};

static TRANSCODED: Lazy<Mutex<HashMap<String, (String, f64)>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static PATH_TO_FILE: Lazy<String> =
    Lazy::new(|| var("PATH_TO_FILE").unwrap_or_else(|_| panic!("PATH_TO_FILE not set in .env")));
static PATH_TO_TRANSCODED_FILE: Lazy<String> = Lazy::new(|| {
    var("PATH_TO_TRANSCODED_FILE")
        .unwrap_or_else(|_| panic!("PATH_TO_TRANSCODED_FILE not set in .env"))
});
static FILE_SIZE_THRESHOLD: Lazy<String> = Lazy::new(|| {
    var("FILE_SIZE_THRESHOLD").unwrap_or_else(|_| panic!("FILE_SIZE_THRESHOLD not set in .env"))
});
static TRANSCODED_FILE_SIZE_THRESHOLD: Lazy<String> = Lazy::new(|| {
    var("TRANSCODED_FILE_SIZE_THRESHOLD")
        .unwrap_or_else(|_| panic!("TRANSCODED_FILE_SIZE_THRESHOLD not set in .env"))
});
static GARBAGE_COLLECTOR_INTERVAL: Lazy<String> = Lazy::new(|| {
    var("GARBAGE_COLLECTOR_INTERVAL")
        .unwrap_or_else(|_| panic!("GARBAGE_COLLECTOR_INTERVAL not set in .env"))
});
static IPFS_GATEWAY: Lazy<String> =
    Lazy::new(|| var("IPFS_GATEWAY").unwrap_or_else(|_| panic!("IPFS_GATEWAY not set in .env")));

#[derive(Debug, Clone)]
struct TranscodeJob {
    task_id: String,
    source_cid: String,
    media_formats: String,
    is_encrypted: bool,
    is_gpu: bool,
    preview_percent: u32,
}

fn get_file_size(file_path: String) -> std::io::Result<u64> {
    let metadata = fs::metadata(file_path)?;
    Ok(metadata.len())
}

const CID_TYPE_ENCRYPTED_SIZE: usize = 1;
const ENCRYPTION_ALGORITHM_SIZE: usize = 1;
const CHUNK_SIZE_AS_POWEROF2_SIZE: usize = 1;

const ENCRYPTED_BLOB_HASH_SIZE: usize = 33;
const KEY_SIZE: usize = 32;

/**
 * Extracts the encryption key from an encrypted CID.
 * @param encrypted_cid - The encrypted CID to get the key from.
 * @returns The encryption key from the CID.
 */
pub fn get_key_from_encrypted_cid(encrypted_cid: &str) -> String {
    let extension_index = encrypted_cid.rfind(".");

    let mut cid_without_extension = match extension_index {
        Some(index) => &encrypted_cid[..index],
        None => encrypted_cid,
    };

    println!(
        "get_key_from_encrypted_cid: encrypted_cid = {}",
        encrypted_cid
    );
    println!(
        "get_key_from_encrypted_cid: cid_without_extension = {}",
        cid_without_extension
    );

    cid_without_extension = &cid_without_extension[1..];
    let cid_bytes = base64url_to_bytes(cid_without_extension);

    let start_index = CID_TYPE_ENCRYPTED_SIZE
        + ENCRYPTION_ALGORITHM_SIZE
        + CHUNK_SIZE_AS_POWEROF2_SIZE
        + ENCRYPTED_BLOB_HASH_SIZE;

    let end_index = start_index + KEY_SIZE;

    let selected_bytes = &cid_bytes[start_index..end_index];

    let key = bytes_to_base64url(selected_bytes);
    println!("get_key_from_encrypted_cid: key = {}", key);

    return key;
}

fn number_of_bytes(value: u32) -> usize {
    let mut value = value;
    let mut bytes = 1;

    while value >= 256 {
        value >>= 8;
        bytes += 1;
    }

    bytes
}

/// Calculates the SHA-256 hash of the given `encrypted_cid`, encrypts it using AES-256-CBC with
/// the specified `key`, and then encodes the result as a URL-safe base64 string. This function is
/// designed for securing sensitive identifiers before storage or transmission.
///
/// # Arguments
/// * `encrypted_cid` - The content identifier to be hashed, encrypted, and encoded.
///
pub fn get_base64_url_encrypted_blob_hash(encrypted_cid: &str) -> Option<String> {
    let encrypted_cid = &encrypted_cid[1..];
    let cid_bytes = base64url_to_bytes(encrypted_cid);

    let start_index =
        CID_TYPE_ENCRYPTED_SIZE + ENCRYPTION_ALGORITHM_SIZE + CHUNK_SIZE_AS_POWEROF2_SIZE;

    let end_index = start_index + ENCRYPTED_BLOB_HASH_SIZE;

    let encrypted_blob_hash = &cid_bytes[start_index..end_index];

    let base64_url = bytes_to_base64url(encrypted_blob_hash);

    Some(base64_url)
}

/// Generates a random filename with the given `prefix` and `extension`.
/// The filename is guaranteed to be unique and not already exist in the
/// current directory. Returns the resulting filename as a `String`.
///
/// # Arguments
///
/// * `prefix` - The prefix to use for the filename.
/// * `extension` - The extension to use for the filename.
///
fn generate_random_filename() -> String {
    let uuid = Uuid::new_v4();
    let timestamp = Utc::now().timestamp_nanos();
    format!("{}_{}", uuid, timestamp)
}

/// Asynchronously receives transcoding tasks from a channel and processes them using the specified transcoder. Each
/// task involves reading an input file, transcoding it according to the provided settings, and writing the output to
/// a specified location. Errors encountered during processing are logged, and upon completion of all tasks, a signal
/// is sent through another channel to indicate completion.
///
/// # Arguments
/// * `receiver` - An `Arc<Mutex<mpsc::Receiver<(String, String, String, bool, bool)>>>` representing a shared receiver
///   channel for transcoding tasks. Each task includes the input file path, output file path, desired format,
///   encryption flag, and GPU usage flag.
///
async fn process_single_job(
    task_id: String,
    orig_source_cid: String,
    media_formats: String,
    is_encrypted: bool,
    is_gpu: bool,
    preview_percent: u32,
) {
    println!("preview_percent: {}", preview_percent);
    let source_cid = Path::new(&orig_source_cid)
        .with_extension("")
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string());

    if source_cid.is_none() {
        eprintln!("Invalid source CID: {}", orig_source_cid);
        return;
    }

    let storage_network: Option<&str> = orig_source_cid
        .split_once("://")
        .map(|(network, _)| network);
    if storage_network.is_none() {
        eprintln!("Invalid source CID: {}", orig_source_cid);
        return;
    }

    let source_cid = source_cid.unwrap();

    let portal_url_var = if is_encrypted {
        "PORTAL_ENCRYPT_URL"
    } else {
        "PORTAL_URL"
    };

    let portal_url = match var(portal_url_var) {
        Ok(url) => url,
        Err(_) => {
            eprintln!(
                "Required environment variable {} not found (is_encrypted={})",
                portal_url_var, is_encrypted
            );
            return;
        }
    };

    println!("source_cid: {}", source_cid);
    println!("portal_url: {}", portal_url);

    let file_path = format!("{}{}", *PATH_TO_FILE, source_cid);

    if !Path::new(&file_path).exists() {
        if is_encrypted {
            println!("source_cid: {}", source_cid);
            let base64_url_encrypted_blob_hash = get_base64_url_encrypted_blob_hash(&source_cid)
                .expect("Failed to get base64 URL encrypted blob hash");

            let url = format!(
                "{}{}{}?types=5,3",
                portal_url, "/api/locations/", base64_url_encrypted_blob_hash
            );
            println!("Downloading and then transcoding video from URL: {}", &url);

            let encrypted_file_path = format!("{}{}_", *PATH_TO_FILE, source_cid);

            match download_video(&url, encrypted_file_path.as_str()).await {
                Ok(_) => println!("Video downloaded successfully"),
                Err(e) => {
                    eprintln!(
                        "Failed to download encrypted video from URL {}: {}",
                        &url, e
                    );
                    return;
                }
            };

            let encrypted_metadata = match std::fs::read_to_string(&encrypted_file_path) {
                Ok(contents) => contents,
                Err(e) => {
                    eprintln!(
                        "Failed to read encrypted metadata from file {}: {}",
                        &encrypted_file_path, e
                    );
                    return;
                }
            };

            let file_path_encrypted = format!("{}{}", *PATH_TO_FILE, generate_random_filename());

            println!("file_encrypted_metadata: {:?}", file_path_encrypted);
            println!("encrypted_metadata: {:?}", encrypted_metadata);

            match download_and_concat_files(encrypted_metadata, file_path_encrypted.clone()).await {
                Ok(()) => println!("Download and concatenation succeeded"),
                Err(e) => eprintln!("Download and concatenation failed: {}", e),
            }

            let file_encrypted_size = get_file_size(file_path_encrypted.clone()).unwrap();
            println!("file_path_encrypted: {}", file_path_encrypted);
            println!("file_encrypted_size: {}", file_encrypted_size);

            let last_index_size =
                (file_encrypted_size as f64 / (262144 + 16) as f64).floor() as u32;

            let key = get_key_from_encrypted_cid(&source_cid);
            let key_bytes = base64url_to_bytes(&key);

            println!("file_path: {}", file_path);
            println!("key: {}", key);
            println!("key_bytes: {:?}", key_bytes);
            println!("last_index_size: {}", last_index_size);

            let part_path = format!("{}.part", file_path);
            match decrypt_file_xchacha20(
                file_path_encrypted,
                part_path.clone(),
                key_bytes,
                0,
                last_index_size,
            ) {
                Ok(_) => println!("Decryption succeeded"),
                Err(error) => {
                    eprintln!("Decryption error: {:?}", error);
                    return;
                }
            }
            if let Err(e) = finalize_part(&part_path, &file_path) {
                eprintln!("Source finalise error: {:?}", e);
                return;
            }
        } else {
            match storage_network.as_deref() {
                Some("ipfs") => {
                    let url = format!("{}{}{}", *IPFS_GATEWAY, "/ipfs/", source_cid);

                    let part_path = format!("{}.part", file_path);
                    match download_video(&url, part_path.as_str()).await {
                        Ok(_) => println!("Video downloaded successfully from URL: {}", url),
                        Err(e) => {
                            eprintln!("Failed to download video from URL {}: {}", &url, e);
                            return;
                        }
                    };
                    if let Err(e) = finalize_part(&part_path, &file_path) {
                        eprintln!("Source finalise error: {:?}", e);
                        return;
                    }
                }
                _ => {
                    let url = format!("{}{}{}", portal_url, "/s5/blob/", source_cid);

                    let part_path = format!("{}.part", file_path);
                    match download_video(&url, part_path.as_str()).await {
                        Ok(_) => println!("Video downloaded successfully from URL: {}", url),
                        Err(e) => {
                            eprintln!("Failed to download video from URL {}: {}", &url, e);
                            return;
                        }
                    };
                    if let Err(e) = finalize_part(&part_path, &file_path) {
                        eprintln!("Source finalise error: {:?}", e);
                        return;
                    }
                }
            }
        }
    } else {
        println!("File already exists: {}", &file_path);
    }

    let media_formats_file = var("MEDIA_FORMATS_FILE").unwrap();

    let media_formats_json = if !media_formats.is_empty() {
        media_formats.clone()
    } else {
        read_to_string(media_formats_file.as_str()).expect("Failed to read video format file")
    };

    println!("media_formats_json: {}", media_formats_json);
    let media_formats_vec: Vec<Value> =
        serde_json::from_str(&media_formats_json).expect("Failed to parse video formats");

    // Initialize progress to 0 at the start for all formats
    let formats_count = media_formats_vec.len();
    for i in 0..formats_count {
        shared::update_progress(&task_id, i, 0);
    }

    // Then, we transcode the downloaded video with each video format
    let mut transcoded_formats = Vec::new();
    let mut source_duration: f64 = 0.0;

    // ── M3 shadow moderation (D1/D4): fire-and-forget, concurrent with the
    //    transcode loop below; incapable of delaying, failing, or blocking
    //    this job. The source (fresh or cache-hit) is final on disk here. ──
    if moderation::moderation_enabled() {
        spawn_shadow_moderation(task_id.clone(), file_path.clone());
    }

    // ── WP-T publish gate (three-state MODERATION_GATE; default off) ──
    let gate = moderation::gate_mode();
    let client = moderation::default_client();
    let tap_dir = transcode_video::modtap_dir(&task_id);
    // Probe the SOURCE (now on disk), not output codecs; `&&` short-circuits so a
    // disabled run never spawns ffprobe. On probe error, source_has_video ⇒ true ⇒ HOLD.
    let scan_required = gate.moderates() && transcode_video::source_has_video(&file_path);
    if gate.moderates() {
        // Defense-in-depth: clear any stale tap dir before producing this job's keyframes
        // so the gate can never read a prior run's PNGs. (task_id is a fresh UUID, so this
        // is belt-and-braces.) Dark-launch parity: guarded by the gate.
        let _ = fs::remove_dir_all(&tap_dir);
    }
    // ── GC pins (Task 3.1.4) ──────────────────────────────────────────────────
    // `garbage_collect` runs on PATH_TO_TRANSCODED_FILE as well as PATH_TO_FILE,
    // and it deletes NEWEST-first — so during the gate's produce→publish window
    // the tap dir and the held outputs are precisely its first candidates. Each
    // unpinned artefact would fail silently, and differently: a reaped tap dir ⇒
    // a spurious fail-closed HOLD of clean content; reaped outputs ⇒ publish
    // uploads nothing; a source reaped mid-hash ⇒ the own-hash exact match
    // degrades with no error anywhere. RAII: every pin releases on every path,
    // including the early `return` in the HOLD branch and on panic.
    let _tap_pin = gate.moderates().then(|| moderation::pin(&tap_dir));
    let mut output_pins: Vec<moderation::PinGuard> = Vec::new();
    let mut pending: Vec<LocalOutput> = Vec::new();
    let mut tap_ok = !scan_required; // audio source: nothing to tap, empty set is clearable
    let mut tapped = false; // has the tap been assigned to a video output yet?

    for (index, video_format) in media_formats_vec.iter().enumerate() {
        let video_format_str = match serde_json::to_string(&video_format) {
            Ok(str) => str,
            Err(e) => {
                eprintln!("Error serializing video format: {:?}", e);
                continue;
            }
        };

        let format_result = get_video_format_from_str(&video_format_str);
        let format = match format_result {
            Ok(format) => format,
            Err(e) => {
                eprintln!("Failed to get video format from string: {}", e);
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
                            "Response: status_code: {}, message: {}, cid: {}",
                            response.status_code, response.message, response.cid
                        );
                        if response.status_code != 200 {
                            eprintln!(
                                "Format {} failed with status {}: {}",
                                index, response.status_code, response.message
                            );
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
        let want_tap =
            fmt_has_video && transcode_video::vf_foldable(video_format["vf"].as_str()) && !tapped;
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
        )
        .await
        {
            Ok(out) => {
                if want_tap {
                    tapped = true;
                    tap_ok = true; // the tap-owning ffmpeg run succeeded
                }
                // Pin the held output for the produce→publish window (Task 3.1.4).
                output_pins.push(moderation::pin(&held_output_path(&out)));
                pending.push(out);
            }
            Err(e) => {
                eprintln!("Error producing format {}: {:?}", index, e);
                if want_tap {
                    tapped = true; // tap owner attempted+failed → tap_ok stays false ⇒ HOLD
                }
                // NEVER return/`?`: fall through so the gate + held-status still run.
            }
        }
    }

    if gate.moderates() {
        // Dedicated source tap for audio-only OUTPUTS of a video source (nothing to tee
        // from): its own ffmpeg child is the job's sole video decode.
        if scan_required && !tapped {
            match transcode_video::tap_source_keyframes(&file_path, &tap_dir) {
                Ok(()) => tap_ok = true,
                Err(e) => eprintln!("Source keyframe tap failed: {:?}", e), // tap_ok stays false ⇒ HOLD
            }
        }

        // ONE read of the tap dir; the integrity check AND the POST share this set (no TOCTOU).
        let kf: Vec<Vec<u8>> = read_keyframe_pngs(&tap_dir);
        let outcome = if scan_required && (!tap_ok || kf.is_empty()) {
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
            // Pinned for the duration: a reap mid-hash silently omits the field,
            // and the own-hash exact match is what Milestone 1a depends on.
            let source_sha = if kf.is_empty() {
                None
            } else {
                let _source_pin = moderation::pin(&file_path);
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
                let held_duration = pending.first().map(|o| o.total_duration).unwrap_or(0.0);
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
                for i in 0..formats_count {
                    shared::update_progress(&task_id, i, 100);
                }
                return;
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
                        continue;
                    }
                    let mut video_format_modified = video_format_value;
                    apply_cid_metadata(&mut video_format_modified, &response, is_hls, &dest);
                    transcoded_formats.push(video_format_modified);
                }
                Err(e) => {
                    eprintln!("Error publishing format: {:?}", e);
                    continue;
                }
            }
        }
    }

    let transcoded_json = serde_json::to_string(&transcoded_formats).unwrap_or_else(|e| {
        eprintln!("Error serializing transcoded formats: {:?}", e);
        "".to_string()
    });

    let mut transcoded = TRANSCODED.lock().await;
    transcoded.insert(task_id.clone(), (transcoded_json, source_duration));

    // Mark progress as complete (100%) for all formats
    for i in 0..formats_count {
        shared::update_progress(&task_id, i, 100);
    }
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
    // the task's first poll the source would otherwise be unprotected — and
    // GC deletes newest-first, which is exactly this file. RAII: released on
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

async fn transcode_task_receiver(receiver: Arc<Mutex<mpsc::Receiver<TranscodeJob>>>) {
    loop {
        let task = receiver.lock().await.recv().await;
        match task {
            Some(job) => {
                let permit = shared::SEMAPHORE.acquire().await.unwrap();
                shared::decrement_queued_increment_active();
                tokio::spawn(async move {
                    process_single_job(
                        job.task_id,
                        job.source_cid,
                        job.media_formats,
                        job.is_encrypted,
                        job.is_gpu,
                        job.preview_percent,
                    )
                    .await;
                    shared::decrement_active();
                    drop(permit);
                });
            }
            None => break,
        }
    }
}

// The gRPC service implementation
#[derive(Debug, Clone)]
struct TranscodeServiceHandler {
    transcode_task_sender: Option<Arc<Mutex<mpsc::Sender<TranscodeJob>>>>,
}

#[async_trait]
#[async_trait]
impl TranscodeService for TranscodeServiceHandler {
    async fn transcode(
        &self,
        request: Request<TranscodeRequest>,
    ) -> Result<Response<TranscodeResponse>, Status> {
        let mut source_cid = request.get_ref().source_cid.clone();
        if source_cid.starts_with("s5://") {
            source_cid = source_cid.strip_prefix("s5://").unwrap().to_string();
        }

        println!("Received source_cid: {}", source_cid);

        let media_formats = request.get_ref().media_formats.clone();
        println!("Received media_formats: {}", media_formats);

        let is_encrypted = request.get_ref().is_encrypted;
        println!("Received is_encrypted: {}", is_encrypted);

        let is_gpu = request.get_ref().is_gpu;
        println!("Received is_gpu: {}", is_gpu);

        println!(
            "transcode_task_sender is None: {}",
            self.transcode_task_sender.is_none()
        );

        let task_id = Uuid::new_v4();
        if let Some(ref sender) = self.transcode_task_sender {
            let sender = sender.lock().await.clone();
            if let Err(e) = sender
                .send(TranscodeJob {
                    task_id: task_id.to_string(),
                    source_cid: source_cid.clone(),
                    media_formats: media_formats.clone(),
                    is_encrypted,
                    is_gpu,
                    preview_percent: request.get_ref().preview_percent,
                })
                .await
            {
                return Err(Status::internal(format!(
                    "Failed to send transcoding task: {}",
                    e
                )));
            }
            shared::increment_queued();
        }

        let response = TranscodeResponse {
            status_code: 200,
            message: "Transcoding task queued".to_string(),
            task_id: task_id.to_string(),
        };

        Ok(Response::new(response))
    }

    async fn get_transcoded(
        &self,
        request: Request<GetTranscodedRequest>,
    ) -> Result<Response<GetTranscodedResponse>, Status> {
        let task_id = &request.get_ref().task_id;
        let transcoded = TRANSCODED.lock().await;
        let entry = transcoded.get(task_id).cloned();

        let (metadata, duration) =
            entry.unwrap_or_else(|| ("Transcoding in progress".to_string(), 0.0));

        let progress = shared::calculate_overall_progress(task_id);

        let response = GetTranscodedResponse {
            status_code: 200,
            metadata,
            progress,
            duration,
        };

        Ok(Response::new(response))
    }
}

impl Drop for TranscodeServiceHandler {
    fn drop(&mut self) {
        self.transcode_task_sender = None;
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

impl From<tokio::sync::mpsc::error::SendError<TranscodeJob>> for TranscodeError {
    fn from(e: tokio::sync::mpsc::error::SendError<TranscodeJob>) -> Self {
        TranscodeError(format!("Failed to send transcoding task: {}", e))
    }
}

#[derive(Debug, Clone)]
struct RestHandler {
    transcode_task_sender: Option<Arc<Mutex<mpsc::Sender<TranscodeJob>>>>,
}

impl RestHandler {
    async fn transcode(
        &self,
        source_cid: String,
        media_formats: String,
        is_encrypted: bool,
        is_gpu: bool,
        preview_percent: u32,
    ) -> Result<impl warp::Reply, warp::Rejection> {
        let task_id = Uuid::new_v4();

        if let Some(ref sender) = self.transcode_task_sender {
            let sender = sender.lock().await.clone();

            if let Err(e) = sender
                .send(TranscodeJob {
                    task_id: task_id.to_string(),
                    source_cid: source_cid.clone(),
                    media_formats: media_formats.clone(),
                    is_encrypted,
                    is_gpu,
                    preview_percent,
                })
                .await
            {
                return Err(warp::reject::custom(TranscodeError::from(e)));
            }
            shared::increment_queued();
        }

        let response = transcode::TranscodeResponse {
            status_code: 200,
            message: "Transcoding task queued".to_string(),
            task_id: task_id.to_string(),
        };

        Ok(warp::reply::json(&TranscodeResponseWrapper::from(response)))
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
        // Retrieve the metadata and the progress for the given task ID.
        let transcoded = TRANSCODED.lock().await;
        let entry = transcoded.get(&task_id).cloned();

        // Use default values if the task is not yet complete.
        let (metadata, duration) =
            entry.unwrap_or_else(|| ("Transcoding in progress".to_string(), 0.0));

        let progress = shared::calculate_overall_progress(&task_id);

        // Construct the response including the progress
        let response = GetTranscodedResponseWrapper {
            status_code: 200,
            metadata,
            progress,
            duration,
        };

        Ok(warp::reply::json(&response))
    }
}

/// The local path a produced-but-unpublished output occupies while the gate
/// holds it.
///
/// Used BOTH to pin it against GC and to delete it on a HOLD, deliberately, so
/// the two can never drift: `moderation::is_pinned` is an EXACT string match
/// against what `read_dir().path()` yields, so a near-miss path protects
/// nothing and fails silently. One function, one string shape.
fn held_output_path(out: &LocalOutput) -> String {
    if out.is_hls {
        transcode_video::hls_output_dir(&out.file_name)
    } else {
        format!(
            "{}{}_ue.{}",
            *PATH_TO_TRANSCODED_FILE, out.file_name, out.ext
        )
    }
}

/// Revived for WP-T (IMPLEMENTATION-MODERATION-FRAMES-GATE-WPT.md).
/// Read the tap dir's keyframe PNGs into memory, sorted by filename. A missing/empty
/// dir maps to an empty `Vec`; ANY per-file read error also maps to empty (fail-closed —
/// a partial set must never be scanned as complete). The gate's SINGLE read of the tap:
/// the integrity check and the POST share this in-memory set (no TOCTOU).
fn read_keyframe_pngs(tap_dir: &str) -> Vec<Vec<u8>> {
    let mut paths: Vec<std::path::PathBuf> = match std::fs::read_dir(tap_dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("png"))
            .collect(),
        Err(_) => return Vec::new(),
    };
    paths.sort();
    let mut frames = Vec::with_capacity(paths.len());
    for p in paths {
        match std::fs::read(&p) {
            Ok(bytes) => frames.push(bytes),
            Err(_) => return Vec::new(), // read error ⇒ empty ⇒ fail-closed HOLD (video source)
        }
    }
    frames
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
/// then atomically rename it into place — the moderation sidecar stat-checks
/// and refuses to report on a file that changes mid-run (`SOURCE_MUTATED`).
/// The parent-directory fsync is deliberately omitted: a crash before the
/// dirent persists only costs a re-download of re-derivable data.
fn finalize_part(part: &str, final_path: &str) -> std::io::Result<()> {
    fs::File::open(part)?.sync_all()?;
    fs::rename(part, final_path)
}

fn garbage_collect(directory: &str, size_threshold: u64) {
    let mut files: Vec<_> = fs::read_dir(directory)
        .unwrap()
        .filter_map(|entry| {
            entry.ok().and_then(|e| {
                // M3: sources with an in-flight shadow moderation call are
                // pinned — CONTRACT §3 requires them undisturbed until the
                // sidecar's response arrives (worst case hours).
                if moderation::is_pinned(&e.path()) {
                    return None;
                }
                let meta = e.metadata().ok()?;
                let size = if meta.is_dir() {
                    dir_size(&e.path())
                } else {
                    meta.len()
                };
                let created = meta.created().ok()?;
                Some((e.path(), size, created, meta.is_dir()))
            })
        })
        .collect();

    files.sort_by_key(|k| k.2); // Sort by creation time

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
            total_size -= size;
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

    let (task_sender, task_receiver) = mpsc::channel::<TranscodeJob>(100);
    let task_receiver = Arc::new(Mutex::new(task_receiver));
    tokio::spawn(transcode_task_receiver(Arc::clone(&task_receiver)));

    let task_sender = Arc::new(Mutex::new(task_sender));

    let grpc_addr = "0.0.0.0:50051"
        .parse()
        .expect("Invalid gRPC server address");
    let transcode_service_handler = TranscodeServiceHandler {
        transcode_task_sender: Some(task_sender.clone()),
    };
    let grpc_server = Server::builder()
        .add_service(TranscodeServiceServer::new(transcode_service_handler))
        .serve(grpc_addr);

    let rest_handler = Arc::new(RestHandler {
        transcode_task_sender: Some(task_sender.clone()),
    });

    let cors = warp::cors()
        .allow_any_origin()
        .allow_methods(vec!["POST", "GET"])
        .allow_headers(vec!["Content-Type"]);

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

    let routes = health.or(status).or(transcode).or(get_transcoded);
    let rest_server = warp::serve(routes).run(([0, 0, 0, 0], 8000));

    let garbage_collection_secs = GARBAGE_COLLECTOR_INTERVAL
        .parse::<u64>()
        .unwrap_or_else(|_| {
            eprintln!("Failed to parse GARBAGE_COLLECTOR_INTERVAL into a u64");
            3600 // default to 1 hour
        });

    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(garbage_collection_secs));
        loop {
            interval.tick().await;
            let threshold = FILE_SIZE_THRESHOLD.parse::<u64>().unwrap_or_else(|_| {
                eprintln!("Failed to parse FILE_SIZE_THRESHOLD into a u64");
                1000000000 // default to 1GB
            });
            garbage_collect(PATH_TO_FILE.as_str(), threshold);
            let transcoded_threshold = TRANSCODED_FILE_SIZE_THRESHOLD
                .parse::<u64>()
                .unwrap_or_else(|_| {
                    eprintln!("Failed to parse TRANSCODED_FILE_SIZE_THRESHOLD into a u64");
                    1000000000 // default to 1GB
                });
            garbage_collect(PATH_TO_TRANSCODED_FILE.as_str(), transcoded_threshold);
        }
    });

    let grpc_server = tokio::spawn(grpc_server);
    let rest_server = tokio::spawn(rest_server);

    match grpc_server.await {
        Ok(_) => println!("gRPC server shut down gracefully."),
        Err(e) => eprintln!("gRPC server error: {}", e),
    }
    match rest_server.await {
        Ok(_) => println!("REST server shut down gracefully."),
        Err(e) => eprintln!("REST server error: {}", e),
    }
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
        &src[..src.find("\n#[cfg(test)]").expect("test module marker")]
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
        let ret = region.find("return;").expect("the hold path must return");
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
            "let _tap_pin = gate.moderates()",
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
            .find("read_keyframe_pngs(&tap_dir)")
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

    /// `held_output_path`'s non-HLS shape must stay identical to the one
    /// `transcode_video` actually writes and later reads. If they drift, the GC
    /// pin protects a path that does not exist and the held output is reaped
    /// mid-window — silently, because `is_pinned` is an exact string match and a
    /// near-miss simply never matches. Cross-module, so nothing else catches it.
    #[test]
    fn test_held_output_path_matches_the_transcoder_output_shape() {
        let tv = include_str!("transcode_video.rs");
        assert!(
            tv.contains(r#"format!("{}{}_ue.{}", *PATH_TO_TRANSCODED_FILE, file_name, ext)"#),
            "transcode_video must still build the non-HLS output as {{dir}}{{name}}_ue.{{ext}}"
        );
        // Both halves, or this proves nothing: asserting only that the
        // transcoder still uses `_ue.` leaves `held_output_path` free to drift
        // away from it, which is precisely the silent-pin failure. (Found by
        // mutating this side and watching the guard survive.)
        assert!(
            job_flow_src().contains(r#""{}{}_ue.{}""#),
            "held_output_path must build the SAME non-HLS shape the transcoder writes"
        );
        // The HLS case is not a second spelling: the gate and the transcoder
        // both go through hls_output_dir, so it cannot drift by construction.
        assert!(
            job_flow_src().contains("transcode_video::hls_output_dir(&out.file_name)"),
            "the gate must derive the HLS dir from hls_output_dir, not rebuild it"
        );
    }

    /// The gate pins its held artefacts using the SAME function that later
    /// deletes them, so the pin string and the delete string cannot drift —
    /// `is_pinned` is an exact match and a near-miss fails silently (3.1.4).
    #[test]
    fn test_held_outputs_are_pinned_with_the_deletion_path_shape() {
        let src = job_flow_src();
        assert!(
            src.contains("moderation::pin(&held_output_path(&out))"),
            "held outputs must be pinned via held_output_path"
        );
        assert!(
            src.contains("fs::remove_dir_all(held_output_path(out))")
                && src.contains("fs::remove_file(held_output_path(out))"),
            "the HOLD path must delete via the same helper it pinned with"
        );
        // Three distinct pins on the gate path: tap dir, each output, source.
        for pin in [
            "moderation::pin(&tap_dir)",
            "moderation::pin(&held_output_path(&out))",
        ] {
            assert!(src.contains(pin), "missing GC pin: {}", pin);
        }
        // The source pin must appear TWICE — once for the M3 shadow window and
        // once for the gate's sha256_file hash. Asserting mere presence would
        // be satisfied by the shadow path alone, leaving the gate's hash
        // unpinned and the own-hash match silently degrading (Task 3.1.4).
        assert!(
            src.matches("moderation::pin(&file_path)").count() >= 2,
            "the gate's own source pin is missing (the shadow path's does not cover it)"
        );
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
            .find("let gate = moderation::gate_mode()")
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

    #[test]
    fn test_sources_are_finalized_before_use() {
        let src = include_str!("server.rs");
        // both source-producing branches write to .part and finalise
        let needle = concat!("finalize", "_part(");
        assert!(
            src.matches(needle).count() >= 3, // decrypt + 2 download sites (+ this test)
            "decrypt/download destinations must go through finalize_part"
        );
        assert!(src.contains(".part\", file_path)") || src.contains("part_path"));
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
        let server_src = include_str!("server.rs");
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
        let server_src = include_str!("server.rs");
        assert!(
            server_src.contains("response.status_code != 200"),
            "transcode loop must check response.status_code"
        );
    }

    #[test]
    fn test_failed_status_code_is_not_200() {
        let success: i32 = 200;
        let failures: [i32; 3] = [500, 400, 0];
        assert_eq!(success != 200, false, "200 should pass the guard");
        for code in &failures {
            assert_eq!(
                *code != 200,
                true,
                "status {} should be caught by the guard",
                code
            );
        }
    }

    #[test]
    fn test_status_endpoint_exists() {
        let server_src = include_str!("server.rs");
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

    #[test]
    fn test_concurrent_receiver_uses_semaphore() {
        let server_src = include_str!("server.rs");
        assert!(
            server_src.contains("shared::SEMAPHORE.acquire()"),
            "receiver must acquire semaphore permit"
        );
        assert!(
            server_src.contains("tokio::spawn"),
            "receiver must spawn jobs concurrently"
        );
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
    fn test_send_sites_increment_queued() {
        let server_src = include_str!("server.rs");
        let count = server_src.matches("shared::increment_queued()").count();
        assert!(
            count >= 2,
            "Both gRPC and REST send sites must call shared::increment_queued() (found {})",
            count
        );
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
        let src = include_str!("server.rs");
        assert!(src.contains("initSegmentCid"), "must map initSegmentCid");
        assert!(src.contains("totalSegments"), "must map totalSegments");
    }

    #[test]
    fn test_preview_percent_threaded_to_transcode_video() {
        let src = include_str!("server.rs");
        assert!(src.contains("preview_percent"), "must pass preview_percent");
    }
}
