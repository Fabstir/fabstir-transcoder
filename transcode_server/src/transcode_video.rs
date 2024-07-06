use crate::shared;

use crate::encrypt_file::encrypt_file_xchacha20;
use crate::encrypted_cid::create_encrypted_cid;
use crate::s5::hash_blake3_file;
use crate::s5::upload_video;
use crate::utils::{
    base64url_to_bytes, bytes_to_base64url, download_and_concat_files, download_video,
    hash_bytes_to_cid,
};
use base64::{engine::general_purpose, DecodeError, Engine as _};
use dotenv::var;
use once_cell::sync::Lazy;
use regex::Regex;
use sanitize_filename::sanitize;
use serde::Deserialize;
use serde_json;
use std::error::Error;
use std::fs::metadata;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use tokio::io::AsyncReadExt;
use tonic::{transport::Server, Code, Request, Response, Status};

static PATH_TO_FILE: Lazy<String> =
    Lazy::new(|| var("PATH_TO_FILE").unwrap_or_else(|_| panic!("PATH_TO_FILE not set in .env")));
static PATH_TO_TRANSCODED_FILE: Lazy<String> = Lazy::new(|| {
    var("PATH_TO_TRANSCODED_FILE")
        .unwrap_or_else(|_| panic!("PATH_TO_TRANSCODED_FILE not set in .env"))
});

pub mod transcode {
    tonic::include_proto!("transcode");
}

#[derive(Debug, Clone)]
pub struct TranscodeVideoResponse {
    pub status_code: i32,
    pub message: String,
    pub cid: String,
}

#[derive(Debug, Deserialize)]
pub struct VideoFormat {
    pub id: u32,
    pub ext: String,
    vcodec: Option<String>,
    acodec: Option<String>,
    preset: Option<String>,
    profile: Option<String>,
    ch: Option<u8>,
    vf: Option<String>,
    b_v: Option<String>,
    ar: Option<String>,
    minrate: Option<String>,
    maxrate: Option<String>,
    bufsize: Option<String>,
    gpu: Option<bool>,
    compression_level: Option<u8>,
    pub dest: Option<String>,
}

fn add_arg(cmd: &mut Command, arg: &str, value: Option<&str>) {
    if let Some(value) = value {
        cmd.arg(arg).arg(value);
    }
}

pub fn get_video_format_from_str(video_format: &str) -> Result<VideoFormat, Status> {
    serde_json::from_str::<VideoFormat>(video_format).map_err(|err| {
        Status::new(
            Code::InvalidArgument,
            format!("Invalid video format: {}", err),
        )
    })
}

/// Gets video duration in seconds using `ffprobe`.
///
/// # Arguments
/// * `file_path`: Path to the video file.
///
/// # Returns:
/// `Result<f64, String>` - Duration in seconds or error message.
///
fn get_video_duration(file_path: &str) -> Result<f64, String> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
            file_path,
        ])
        .output()
        .expect("failed to execute ffprobe");

    if output.status.success() {
        let duration_str = String::from_utf8(output.stdout).unwrap();
        duration_str
            .trim()
            .parse::<f64>()
            .map_err(|e| e.to_string())
    } else {
        Err(String::from("Failed to retrieve video duration"))
    }
}

/// Parses ffmpeg progress output to calculate and return the transcoding progress as a percentage.
/// This function searches for time stamps in the ffmpeg output and calculates the progress based
/// on the total duration of the video. If the total duration is not positive, it returns 0 to
/// prevent division by zero errors.
///
/// # Arguments
/// * `line` - A string slice containing a line of ffmpeg output.
/// * `total_duration` - The total duration of the video in seconds.
///
/// # Returns
/// An `Option<i32>` representing the transcoding progress percentage, or `None` if the progress
/// cannot be determined from the given line.
///
fn parse_progress(line: &str, total_duration: f64) -> Option<i32> {
    if total_duration <= 0.0 {
        return Some(0); // Prevent division by zero
    }

    let re = Regex::new(r"time=(\d+):(\d+):(\d+\.\d+)").unwrap();
    if let Some(caps) = re.captures(line) {
        let hours = caps.get(1).unwrap().as_str().parse::<f64>().unwrap_or(0.0);
        let minutes = caps.get(2).unwrap().as_str().parse::<f64>().unwrap_or(0.0);
        let seconds = caps.get(3).unwrap().as_str().parse::<f64>().unwrap_or(0.0);
        let current_time_seconds = hours * 3600.0 + minutes * 60.0 + seconds;
        let progress = ((current_time_seconds / total_duration) * 100.0).round() as i32;
        return Some(progress);
    }

    None
}

/// Executes the ffmpeg command to transcode a video file based on the specified parameters.
/// This function supports GPU acceleration and handles various video formats.
///
/// # Arguments
/// * `task_id` - A unique identifier for the transcoding task.
/// * `format_index` - The index specifying the target video format from a predefined list.
/// * `file_path` - The path to the input video file to be transcoded.
/// * `file_name` - The name of the input video file.
/// * `is_gpu` - A boolean flag indicating whether to use GPU acceleration for transcoding.
/// * `format` - The desired output video format.
/// * `total_duration` - The total duration of the video file in seconds.
///
/// # Returns
/// A `Result<(), Status>` indicating the success or failure of the transcoding operation.
///
fn run_ffmpeg(
    task_id: String,
    format_index: usize,
    file_path: &str,
    file_name: &str,
    is_gpu: bool,
    format: &VideoFormat,
    total_duration: f64,
) -> Result<(), Status> {
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-v").arg("info");
    cmd.arg("-progress").arg("pipe:2");
    cmd.arg("-stats_period").arg("1");

    if is_gpu {
        add_arg(&mut cmd, "-i", Some(file_path));
        add_arg(&mut cmd, "-c:v", format.vcodec.as_deref());
        add_arg(&mut cmd, "-b:v", format.b_v.as_deref());
        add_arg(&mut cmd, "-c:a", Some("libopus"));
        add_arg(&mut cmd, "-b:a", Some("192k"));
        if let Some(ch) = format.ch {
            add_arg(&mut cmd, "-ac", Some(&ch.to_string()));
        }
        add_arg(&mut cmd, "-ar", format.ar.as_deref());
        add_arg(&mut cmd, "-vf", format.vf.as_deref());
        if let Some(ref minrate) = format.minrate {
            cmd.args(["-minrate", minrate]);
        }
        if let Some(ref maxrate) = format.maxrate {
            cmd.args(["-maxrate", maxrate]);
        }
        if let Some(ref bufsize) = format.bufsize {
            cmd.args(["-bufsize", bufsize]);
        }
        cmd.args([
            "-y",
            format!(
                "{}{}_ue.{}",
                *PATH_TO_TRANSCODED_FILE, file_name, format.ext
            )
            .as_str(),
        ]);
    } else {
        if let Some(vcodec) = &format.vcodec {
            if !vcodec.is_empty() {
                add_arg(&mut cmd, "-i", Some(file_path));
                add_arg(&mut cmd, "-c:v", format.vcodec.as_deref());
                add_arg(&mut cmd, "-cpu-used", Some("4"));
                add_arg(&mut cmd, "-b:v", format.b_v.as_deref());
                add_arg(&mut cmd, "-crf", Some("30"));
                add_arg(&mut cmd, "-c:a", Some("libopus"));
                add_arg(&mut cmd, "-b:a", Some("192k"));
                if let Some(ch) = format.ch {
                    add_arg(&mut cmd, "-ac", Some(&ch.to_string()));
                }
                add_arg(&mut cmd, "-vf", format.vf.as_deref());
                add_arg(
                    &mut cmd,
                    "-y",
                    Some(&format!(
                        "{}{}_ue.{}",
                        *PATH_TO_TRANSCODED_FILE, file_name, format.ext
                    )),
                );
            } else {
                return Err(Status::new(
                    Code::InvalidArgument,
                    "No video codec specified",
                ));
            }
        } else if let Some(acodec) = &format.acodec {
            if !acodec.is_empty() {
                add_arg(&mut cmd, "-i", Some(file_path));
                add_arg(&mut cmd, "-acodec", format.acodec.as_deref());
                if let Some(ch) = format.ch {
                    add_arg(&mut cmd, "-ac", Some(&ch.to_string()));
                }
                add_arg(&mut cmd, "-ar", format.ar.as_deref());
                if let Some(compression_level) = format.compression_level {
                    add_arg(
                        &mut cmd,
                        "-compression_level",
                        Some(&compression_level.to_string()),
                    );
                }
                add_arg(
                    &mut cmd,
                    "-y",
                    Some(&format!(
                        "{}{}_ue.{}",
                        *PATH_TO_TRANSCODED_FILE, file_name, format.ext
                    )),
                );
            } else {
                return Err(Status::new(
                    Code::InvalidArgument,
                    "No audio codec specified",
                ));
            }
        } else {
            return Err(Status::new(Code::InvalidArgument, "No codec specified"));
        }
    }

    cmd.stderr(Stdio::piped()).stdout(Stdio::null());

    let mut child = cmd.spawn().expect("failed to start ffmpeg command");

    if let Some(stderr) = child.stderr.take() {
        let reader = BufReader::new(stderr);
        let mut last_progress = 0;
        for line_result in reader.lines() {
            if let Ok(line) = line_result {
                if let Some(progress) = parse_progress(&line, total_duration) {
                    last_progress = progress;
                    shared::update_progress(&task_id, format_index, last_progress);
                }
                println!("£££££ {} £££££", line);
                println!("Progress: {}%", last_progress);
            }
        }
    }

    let output = child.wait().expect("Transcode process wasn't running");
    println!("Transcode finished with status: {}", output);

    Ok(())
}


/// Asynchronously transcodes a video from a given format to another using ffmpeg,
/// based on the specified transcoder settings. This function supports optional
/// encryption and GPU acceleration.
///
/// # Arguments
/// * `task_id` - A unique identifier for the transcoding task.
/// * `format_index` - The index specifying the target video format from a predefined list.
/// * `file_path` - The path to the input video file to be transcoded.
/// * `video_format` - The desired output video format.
/// * `is_encrypted` - A boolean flag indicating whether the output video should be encrypted.
/// * `is_gpu` - A boolean flag indicating whether to use GPU acceleration for transcoding.
///
/// # Returns
/// A `Result` wrapping a `Response` with the `TranscodeVideoResponse` on success,
/// or a `Status` error on failure.
///
pub async fn transcode_video(
    task_id: String,
    format_index: usize,
    file_path: &str,
    video_format: &str,
    is_encrypted: bool,
    is_gpu: bool,
) -> Result<Response<TranscodeVideoResponse>, Status> {
    println!("transcode_video: Processing video at: {}", file_path);
    println!("transcode_video: video_format: {}", video_format);
    println!("transcode_video: is_encrypted: {}", is_encrypted);
    println!("transcode_video: is_gpu: {}", is_gpu);

    let file_name = Path::new(file_path)
        .file_name()
        .ok_or_else(|| Status::new(Code::InvalidArgument, "Invalid file path"))?
        .to_string_lossy()
        .to_string();

    let format = get_video_format_from_str(video_format)?;

    let file_name = format!("{}_{}", file_name, format.id.to_string());

    println!("Transcoding video: {}", &file_path);
    println!("is_gpu = {}", &is_gpu);

    let total_duration = get_video_duration(file_path).unwrap_or_else(|_| 0.0);
    println!("Total video duration: {} seconds", total_duration);

    let mut encryption_key1: Vec<u8> = Vec::new();

    let response: TranscodeVideoResponse;

    run_ffmpeg(
        task_id,
        format_index,
        file_path,
        &file_name,
        is_gpu,
        &format,
        total_duration,
    )?;

    if is_encrypted {
        match encrypt_file_xchacha20(
            format!(
                "{}{}_ue.{}",
                *PATH_TO_TRANSCODED_FILE, file_name, format.ext
            ),
            format!("{}{}.{}", *PATH_TO_TRANSCODED_FILE, file_name, format.ext),
            0,
        ) {
            Ok(bytes) => {
                // Encryption succeeded, and `bytes` contains the encrypted data
                // Add your success handling code here
                encryption_key1 = bytes;
                println!("Encryption succeeded");
            }
            Err(error) => {
                // Encryption failed
                // Handle the error here
                eprintln!("Encryption error: {:?}", error);
                // Optionally, you can return an error or perform error-specific handling
            }
        }

        let file_path = format!(
            "{}{}_ue.{}",
            *PATH_TO_TRANSCODED_FILE, file_name, format.ext
        );
        let file_path_encrypted =
            format!("{}{}.{}", *PATH_TO_TRANSCODED_FILE, file_name, format.ext);

        let hash_result = hash_blake3_file(file_path.clone());
        let hash_result_encrypted = hash_blake3_file(file_path_encrypted.to_owned());

        let cid_type_encrypted: u8 = 0xae; // replace with your actual cid type encrypted
        let encryption_algorithm: u8 = 0xa6; // replace with your actual encryption algorithm
        let chunk_size_as_power_of_2: u8 = 18; // replace with your actual chunk size as power of 2
        let padding: u32 = 0; // replace with your actual padding

        // Upload the transcoded videos to storage
        match upload_video(file_path_encrypted.as_str(), format.dest).await {
            Ok(cid_encrypted) => {
                println!(
                    "****************************************** cid: {:?}",
                    &cid_encrypted
                );

                let mut hash = Vec::new();
                match hash_result {
                    Ok(hash1) => {
                        hash = hash1.as_bytes().to_vec();
                        // Now you can use bytes as needed.
                    }
                    Err(err) => {
                        eprintln!("Error computing blake3 hash: {}", err);

                        return Err(Status::new(
                            Code::Internal,
                            format!("Error computing blake3 hash: {}", err),
                        ));
                    }
                }

                let mut hash_encrypted = Vec::new();
                match hash_result_encrypted {
                    Ok(hash1) => {
                        hash_encrypted = hash1.as_bytes().to_vec();
                        // Now you can use bytes as needed.
                    }
                    Err(err) => {
                        eprintln!("Error computing blake3 hash: {}", err);

                        return Err(Status::new(
                            Code::Internal,
                            format!("Error computing blake3 hash: {}", err),
                        ));
                    }
                }

                let mut encrypted_blob_hash = vec![0x1f];
                encrypted_blob_hash.extend(hash_encrypted);

                let cloned_hash = encrypted_blob_hash.clone();

                let file_path_path = Path::new(&file_path);
                let metadata = std::fs::metadata(file_path_path).expect("Failed to read metadata");
                let file_size = metadata.len();

                let cid = hash_bytes_to_cid(hash, file_size);

                println!("encryption_key1: {:?}", encryption_key1);
                println!("cid_encrypted: {:?}", cid_encrypted);
                println!("cid: {:?}", cid);

                println!(
                    "upload_video Ok: encrypted_blob_hash = {:?}",
                    hex::encode(&encrypted_blob_hash)
                );
                println!(
                    "upload_video Ok: encryption_key1 = {:?}",
                    hex::encode(&encryption_key1)
                );
                println!("upload_video Ok: cid = {:?}", hex::encode(&cid));

                let hash = hash_blake3_file(file_path_encrypted).unwrap();
                println!(
                    "`upload_video: encryptedBlobMHashBase64url` = {}",
                    general_purpose::URL_SAFE_NO_PAD
                        .encode([&[31u8] as &[_], hash.as_bytes()].concat())
                );

                let encrypted_cid_bytes = create_encrypted_cid(
                    cid_type_encrypted,
                    encryption_algorithm,
                    chunk_size_as_power_of_2,
                    encrypted_blob_hash,
                    encryption_key1,
                    padding,
                    cid,
                );

                println!(
                    "upload_video Ok: encrypted_cid_bytes = {:?}",
                    hex::encode(&encrypted_cid_bytes)
                );
                let encrypted_cid = format!("u{}", bytes_to_base64url(&encrypted_cid_bytes));
                println!("upload_video Ok: encrypted_cid = {}", encrypted_cid);

                // Now you have your encrypted_blob_hash and encrypted_cid
                println!("Encrypted Blob Hash: {:02x?}", cloned_hash);
                println!("Encrypted CID: {:?}", encrypted_cid);

                println!("Transcoding task finished");

                // Return the TranscodeVideoResponse with the job ID
                response = TranscodeVideoResponse {
                    status_code: 200,
                    message: String::from("Transcoding successful"),
                    cid: encrypted_cid,
                };
            }
            Err(e) => {
                println!("!!!!!!!!!!!!!!!!!!!!!2160p no cid");
                println!("Error: {}", e); // This line is added to print out the error message

                response = TranscodeVideoResponse {
                    status_code: 500,
                    message: format!("Transcoding task failed with error {}", e),
                    cid: "".to_string(),
                };
            }
        };
    } else {
        let file_path = format!(
            "{}{}_ue.{}",
            *PATH_TO_TRANSCODED_FILE, file_name, format.ext
        );

        // Upload the transcoded videos to storage
        match upload_video(file_path.as_str(), format.dest.clone()).await {
            Ok(cid) => {
                println!("cid: {:?}", cid);

                println!("Transcoding task finished");

                // Return the TranscodeVideoResponse with the job ID
                response = TranscodeVideoResponse {
                    status_code: 200,
                    message: String::from("Transcoding successful"),
                    cid,
                };
            }
            Err(e) => {
                println!("!!!!!!!!!!!!!!!!!!!!!2160p no cid");
                println!("Error: {}", e); // This line is added to print out the error message

                response = TranscodeVideoResponse {
                    status_code: 500,
                    message: format!("Transcoding task failed with error {}", e),
                    cid: "".to_string(),
                };
            }
        };
    }

    Ok(Response::new(response))
}
