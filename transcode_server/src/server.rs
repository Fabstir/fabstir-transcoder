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

mod s5;

mod encrypt_file;

mod utils;
use utils::{base64url_to_bytes, bytes_to_base64url, download_and_concat_files, download_video};

mod transcode_video;
use transcode_video::{get_video_format_from_str, transcode_video, TranscodeVideoResponse};

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

static TRANSCODED: Lazy<Mutex<HashMap<String, String>>> = Lazy::new(|| Mutex::new(HashMap::new()));
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
static IPFS_GATEWAY: Lazy<String> = Lazy::new(|| {
    var("IPFS_GATEWAY")
        .unwrap_or_else(|_| panic!("IPFS_GATEWAY not set in .env"))
});


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
async fn transcode_task_receiver(
    receiver: Arc<Mutex<mpsc::Receiver<(String, String, String, bool, bool)>>>,
) {
    while let Some((task_id, orig_source_cid, media_formats, is_encrypted, is_gpu)) =
        receiver.lock().await.recv().await
    {
        let source_cid = Path::new(&orig_source_cid)
            .with_extension("")
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string());

        if source_cid.is_none() {
            eprintln!("Invalid source CID: {}", orig_source_cid);
            continue;
        }

        let storage_network: Option<&str> = orig_source_cid.split_once("://").map(|(network, _)| network);
        if storage_network.is_none() {
            eprintln!("Invalid source CID: {}", orig_source_cid);
            continue;
        }

        let source_cid = source_cid.unwrap();

        let portal_url_result = if is_encrypted {
            var("PORTAL_ENCRYPT_URL")
        } else {
            var("PORTAL_URL")
        };

        let portal_url = match portal_url_result {
            Ok(url) => url,
            Err(_) => {
                eprintln!("Required environment variable for PORTAL_URL not found");
                continue; // Skip the rest of this loop iteration
            }
        };

        println!("source_cid: {}", source_cid);
        println!("portal_url: {}", portal_url);

        let file_path = format!("{}{}", *PATH_TO_FILE, source_cid);

        if !Path::new(&file_path).exists() {
            if is_encrypted {
                println!("source_cid: {}", source_cid);
                let base64_url_encrypted_blob_hash =
                    get_base64_url_encrypted_blob_hash(&source_cid)
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
                        continue;
                    }
                };

                let encrypted_metadata = match std::fs::read_to_string(&encrypted_file_path) {
                    Ok(contents) => contents,
                    Err(e) => {
                        eprintln!(
                            "Failed to read encrypted metadata from file {}: {}",
                            &encrypted_file_path, e
                        );
                        continue;
                    }
                };

                let file_path_encrypted =
                    format!("{}{}", *PATH_TO_FILE, generate_random_filename());

                println!("file_encrypted_metadata: {:?}", file_path_encrypted);
                println!("encrypted_metadata: {:?}", encrypted_metadata);

                match download_and_concat_files(encrypted_metadata, file_path_encrypted.clone())
                    .await
                {
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

                match decrypt_file_xchacha20(
                    file_path_encrypted,
                    file_path.clone(),
                    key_bytes,
                    0,
                    last_index_size,
                ) {
                    Ok(_) => println!("Decryption succeeded"),
                    Err(error) => {
                        eprintln!("Decryption error: {:?}", error);
                        continue;
                    }
                }
            } else {
                match storage_network.as_deref() {
                    Some("ipfs") => {
                        let url = format!("{}{}{}", *IPFS_GATEWAY, "/ipfs/", source_cid);

                        match download_video(&url, file_path.as_str()).await {
                            Ok(_) => println!("Video downloaded successfully"),
                            Err(e) => {
                                eprintln!("Failed to download video from URL {}: {}", &url, e);
                                continue;
                            }
                        };                    
                    },
                    _ => 
                    {
                        let url = format!("{}{}{}", portal_url, "/s5/blob/", source_cid);

                        match download_video(&url, file_path.as_str()).await {
                            Ok(_) => println!("Video downloaded successfully"),
                            Err(e) => {
                                eprintln!("Failed to download video from URL {}: {}", &url, e);
                                continue;
                            }
                        };        
                    },
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

            if !check_transcoded_file_exists(
                file_path.as_str(),
                &format.id.to_string(),
                format.ext.as_str(),
            )
            .await
            {
                let transcode_result: std::prelude::v1::Result<
                    Response<TranscodeVideoResponse>,
                    Status,
                > = transcode_video(
                    task_id.clone(),
                    index,
                    &file_path,
                    &video_format_str,
                    is_encrypted,
                    is_gpu,
                )
                .await;

                match transcode_result {
                    Ok(transcode_video_response) => {
                        // Handle the successful response
                        let response = transcode_video_response.into_inner();
                        println!(
                            "Response: status_code: {}, message: {}, cid: {}",
                            response.status_code, response.message, response.cid
                        );

                        // Create a mutable clone of video_format
                        let mut video_format_modified = video_format.clone();

                        match &format.dest {
                            Some(dest) if dest == "ipfs" => {
                                video_format_modified["cid"] =
                                    json!(format!("ipfs://{}", response.cid));
                            }
                            _ => {
                                video_format_modified["cid"] =
                                    json!(format!("s5://{}", response.cid));
                            }
                        }
                        transcoded_formats.push(video_format_modified);
                    }
                    Err(e) => {
                        // Log the error and continue with the next format
                        eprintln!("Error transcoding video: {:?}", e);
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
        transcoded.insert(task_id.clone(), transcoded_json);

        // Mark progress as complete (100%) for all formats
        for i in 0..formats_count {
            shared::update_progress(&task_id, i, 100);
        }
    }
}


// The gRPC service implementation
#[derive(Debug, Clone)]
struct TranscodeServiceHandler {
    transcode_task_sender: Option<Arc<Mutex<mpsc::Sender<(String, String, String, bool, bool)>>>>,
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
                .send((
                    task_id.to_string(),
                    source_cid.clone(),
                    media_formats.clone(),
                    is_encrypted,
                    is_gpu,
                ))
                .await
            {
                return Err(Status::internal(format!(
                    "Failed to send transcoding task: {}",
                    e
                )));
            }
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
        let metadata_option = transcoded.get(task_id).cloned();

        let metadata = metadata_option.unwrap_or_else(|| "Transcoding in progress".to_string());

        let progress = shared::calculate_overall_progress(task_id);

        let response = GetTranscodedResponse {
            status_code: 200,
            metadata,
            progress,
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

impl From<tokio::sync::mpsc::error::SendError<(String, String, String, bool, bool)>>
    for TranscodeError
{
    fn from(e: tokio::sync::mpsc::error::SendError<(String, String, String, bool, bool)>) -> Self {
        TranscodeError(format!("Failed to send transcoding task: {}", e))
    }
}

#[derive(Debug, Clone)]
struct RestHandler {
    transcode_task_sender: Option<Arc<Mutex<mpsc::Sender<(String, String, String, bool, bool)>>>>,
}

impl RestHandler {
    async fn transcode(
        &self,
        source_cid: String,
        media_formats: String,
        is_encrypted: bool,
        is_gpu: bool,
    ) -> Result<impl warp::Reply, warp::Rejection> {
        let task_id = Uuid::new_v4();

        if let Some(ref sender) = self.transcode_task_sender {
            let sender = sender.lock().await.clone();

            if let Err(e) = sender
                .send((
                    task_id.to_string(),
                    source_cid.clone(),
                    media_formats.clone(),
                    is_encrypted,
                    is_gpu,
                ))
                .await
            {
                return Err(warp::reject::custom(TranscodeError::from(e)));
            }
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
}

impl From<transcode::GetTranscodedResponse> for GetTranscodedResponseWrapper {
    fn from(response: transcode::GetTranscodedResponse) -> Self {
        GetTranscodedResponseWrapper {
            status_code: response.status_code,
            metadata: response.metadata,
            progress: response.progress,
        }
    }
}

impl RestHandler {
    async fn get_transcoded(&self, task_id: String) -> Result<impl warp::Reply, warp::Rejection> {
    // Retrieve the metadata and the progress for the given task ID.
    let transcoded = TRANSCODED.lock().await;
    let metadata_option = transcoded.get(&task_id).cloned();

    // Use a default value for metadata if it's not available.
    let metadata = metadata_option.unwrap_or_else(|| "Transcoding in progress".to_string());

    let progress = shared::calculate_overall_progress(&task_id);

    // Construct the response including the progress
    let response = GetTranscodedResponseWrapper {
        status_code: 200,
        metadata,
        progress,
    };

    Ok(warp::reply::json(&response))
    }
}

async fn check_transcoded_file_exists(cid: &str, label: &str, ext: &str) -> bool {
    let filename = format!("{}{}_{}.{}", *PATH_TO_TRANSCODED_FILE, cid, label, ext); // Adjust the path and format as needed.
    Path::new(&filename).exists()
}

fn garbage_collect(directory: &str, size_threshold: u64) {
    let mut files: Vec<_> = fs::read_dir(directory)
        .unwrap()
        .filter_map(|entry| {
            entry.ok().and_then(|e| {
                e.metadata()
                    .ok()
                    .map(|m| (e.path(), m.len(), m.created().unwrap()))
            })
        })
        .collect();

    files.sort_by_key(|k| k.2); // Sort files by creation time

    let mut total_size: u64 = files.iter().map(|(_, size, _)| size).sum();

    while total_size > size_threshold && !files.is_empty() {
        if let Some((file, size, _)) = files.pop() {
            fs::remove_file(file).unwrap();
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
}

/// The main entry point for the transcode server. Initializes the server
/// with the specified configuration, starts the gRPC server, and listens
/// for incoming requests. Once a request is received, it spawns a new thread
/// to handle the request and continues listening for more requests.
///
#[tokio::main]
async fn main() {
    dotenv().ok();

    let (task_sender, task_receiver) = mpsc::channel::<(String, String, String, bool, bool)>(100);
    let task_receiver = Arc::new(Mutex::new(task_receiver));
    tokio::spawn(transcode_task_receiver(Arc::clone(&task_receiver)));

    let task_sender = Arc::new(Mutex::new(task_sender));

    let grpc_addr = "0.0.0.0:50051".parse().expect("Invalid gRPC server address");
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
                    )
                    .await
            }
        })
        .with(cors.clone())
        .boxed();

    let get_transcoded_handler = Arc::clone(&rest_handler);
    let get_transcoded = warp::path!("get_transcoded" / String)
        .and_then(move |task_id| {
            let rest_handler = Arc::clone(&get_transcoded_handler);
            async move { rest_handler.get_transcoded(task_id).await }
        })
        .with(cors.clone())
        .boxed();

    let routes = transcode.or(get_transcoded);
    let rest_server = warp::serve(routes).run(([0, 0, 0, 0], 8000));

    let garbage_collection_secs = GARBAGE_COLLECTOR_INTERVAL.parse::<u64>().unwrap_or_else(|_| {
        eprintln!("Failed to parse GARBAGE_COLLECTOR_INTERVAL into a u64");
        3600 // default to 1 hour
    });

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(garbage_collection_secs));
        loop {
            interval.tick().await;
            let threshold = FILE_SIZE_THRESHOLD.parse::<u64>().unwrap_or_else(|_| {
                eprintln!("Failed to parse FILE_SIZE_THRESHOLD into a u64");
                1000000000 // default to 1GB
            });
            garbage_collect(PATH_TO_FILE.as_str(), threshold);
            let transcoded_threshold = TRANSCODED_FILE_SIZE_THRESHOLD.parse::<u64>().unwrap_or_else(|_| {
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

