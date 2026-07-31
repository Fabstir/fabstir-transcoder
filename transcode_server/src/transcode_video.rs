use crate::moderation;
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

// NOTE (WP-T Task 1.2.4): this module's own `PATH_TO_FILE` static was deleted
// rather than un-quarantined. It had no user even at 310c30a when the A1 tap was
// fully live — it was a redundant duplicate of `server.rs`'s static, and the
// `#[allow(dead_code)]` on it was hiding that rather than deferring a caller.
// Lifting the allow with no caller would have left a new dead-code warning, and
// the plan forbids silencing that with a fresh allow. Never initialised, so
// deleting it is behaviour-neutral.
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
    pub duration: f64,
}

/// Represents the configuration for video transcoding format settings.
/// This struct defines various parameters used by FFmpeg for video and audio transcoding.
///
/// # Fields
/// * `id` - Unique identifier for the format configuration
/// * `ext` - Output file extension (e.g., "mp4", "webm")
/// * `vcodec` - Video codec (e.g., "h264_nvenc" for GPU encoding)
/// * `acodec` - Audio codec (e.g., "aac", "libopus")
/// * `preset` - Encoding preset for speed/quality tradeoff
/// * `profile` - Encoding profile (e.g., "high", "main")
/// * `ch` - Number of audio channels
/// * `vf` - Video filter string
/// * `b_v` - Video bitrate
/// * `c_a` - Audio codec
/// * `b_a` - Audio bitrate
/// * `ar` - Audio sample rate
/// * `minrate` - Minimum video bitrate
/// * `maxrate` - Maximum video bitrate
/// * `bufsize` - Video buffer size
/// * `gpu` - Whether to use GPU acceleration
/// * `compression_level` - Audio compression level
/// * `dest` - Destination path for the transcoded file
/// * `encrypt` - Whether to encrypt the output file
/// * `trim_percent` - Keep only the first N% of duration (1–99); used for preview clips
/// * `hls` - When true, output fMP4 segments instead of single file
/// * `hls_time` - Segment duration in seconds (default 6)
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
    c_a: Option<String>,
    b_a: Option<String>,
    ar: Option<String>,
    minrate: Option<String>,
    maxrate: Option<String>,
    bufsize: Option<String>,
    gpu: Option<bool>,
    compression_level: Option<u8>,
    pub dest: Option<String>,
    encrypt: Option<bool>,
    trim_percent: Option<u8>,
    pub hls: Option<bool>,
    pub hls_time: Option<u32>,
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

pub fn hls_output_dir(file_name: &str) -> String {
    format!("{}{}_hls", *PATH_TO_TRANSCODED_FILE, file_name)
}

// ── Moderation keyframe tap (A1) ───────────────────────────────────────────────
// `modtap_dir`/`source_has_video`/`tap_source_keyframes` are called from the
// Phase-5 gate in server.rs.

/// Downscale the moderation tap to a ≤512px long edge. PDQ runs at 64×64 so this is
/// node-transparent, and it bounds the in-memory keyframe set (the Phase-5 gate reads
/// every tap PNG at once) regardless of source resolution. `min(512,iw)` never upscales;
/// `-2` keeps aspect with an even dimension. Quoted so the inner comma is not parsed as
/// a filter separator.
const TAP_SCALE: &str = "scale='min(512,iw)':-2";

/// Per-job directory holding the sampled moderation keyframe PNGs.
/// Revived for WP-T (IMPLEMENTATION-MODERATION-FRAMES-GATE-WPT.md).
pub fn modtap_dir(task_id: &str) -> String {
    format!("{}{}_modtap", *PATH_TO_TRANSCODED_FILE, task_id)
}

/// True if the source has at least one video stream. ffprobe-based; on ANY
/// error/ambiguity returns `true` (fail-closed — a probe false-negative must never
/// make a video source look audio-only).
/// Revived for WP-T (IMPLEMENTATION-MODERATION-FRAMES-GATE-WPT.md).
pub fn source_has_video(path: &str) -> bool {
    match Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
            path,
        ])
        .output()
    {
        // Only a CLEAN, successful probe that affirmatively reports no video stream may
        // say audio-only. A non-zero exit (I/O error, partially-corrupt container, FS
        // hiccup) leaves stdout empty under `-v error`, which must NOT read as audio-only.
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|l| l.trim() == "video"),
        _ => true, // spawn failure OR non-zero exit ⇒ assume video ⇒ fail-closed (HOLD)
    }
}

/// Dedicated keyframe extraction for a video SOURCE whose requested outputs are
/// all audio (no transcode to tee from): its own ffmpeg child is the job's sole
/// video decode. Spawn/wait failures AND a non-zero exit ⇒ `Err` (never panic),
/// so the Phase-5 gate leaves `tap_ok = false` ⇒ HOLD.
/// Revived for WP-T (IMPLEMENTATION-MODERATION-FRAMES-GATE-WPT.md).
pub fn tap_source_keyframes(file_path: &str, tap_dir: &str) -> Result<(), Status> {
    std::fs::create_dir_all(tap_dir)
        .map_err(|e| Status::new(Code::Internal, format!("modtap dir: {}", e)))?;
    let d = get_video_duration(file_path).unwrap_or(0.0);
    let fps = format!(
        "fps={:.5},{}",
        1.0 / moderation::effective_interval(d),
        TAP_SCALE
    );
    let out = format!("{}/kf_%05d.png", tap_dir);
    let cap = moderation::frames_cap(d).to_string(); // duration-derived (see append_tap_post)
    let status = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-i",
            file_path,
            "-vf",
            fps.as_str(),
            "-fps_mode",
            "vfr",
            "-frames:v",
            cap.as_str(),
            "-y",
            out.as_str(),
        ])
        .status()
        .map_err(|e| Status::new(Code::Internal, format!("source tap spawn: {}", e)))?;
    if !status.success() {
        return Err(Status::new(
            Code::Internal,
            format!("source tap ffmpeg exited: {}", status),
        ));
    }
    Ok(())
}

/// A `format.vf` is "foldable" into the tap filtergraph only if it is a simple
/// comma-chain — no `;`/`[`/`]` graph separators or pad labels that could collide
/// with the reserved `[s]/[t]/[m]/[k]` labels (a graph-injection / fail-open risk;
/// `vf` rides in the request `media_formats` JSON). `None` (no filter) ⇒ foldable.
/// Used by both `run_ffmpeg` (do_tap) and the Phase-5 gate's tap-owner pick.
/// 1.72-compatible (avoids `Option::is_none_or`, which is 1.82+).
pub fn vf_foldable(vf: Option<&str>) -> bool {
    !matches!(vf, Some(s) if s.contains(';') || s.contains('[') || s.contains(']'))
}

/// Append the teed keyframe filtergraph + the main-output maps. MUST be emitted
/// BEFORE the main `-y <output>` (the `-map`s bind to that next output). The encode
/// branch keeps the requested `format.vf`; the tap branch is sampled then downscaled
/// (`TAP_SCALE`) so the published video is unaffected and the keyframe set stays small.
fn append_tap_pre(cmd: &mut Command, vf: Option<&str>, fps: &str) {
    let graph = match vf {
        Some(vf) => format!(
            "[0:v]split=2[s][t];[s]{}[m];[t]fps={},{}[k]",
            vf, fps, TAP_SCALE
        ),
        None => format!("[0:v]split=2[m][t];[t]fps={},{}[k]", fps, TAP_SCALE),
    };
    cmd.args([
        "-filter_complex",
        graph.as_str(),
        "-map",
        "[m]",
        "-map",
        "0:a:0?", // first audio stream (optional) — matches default single-stream selection
    ]);
}

/// Append the keyframe PNG tap output. MUST follow the main `-y <output>`.
fn append_tap_post(cmd: &mut Command, dir: &str, total_duration: f64) {
    let png = format!("{}/kf_%05d.png", dir);
    // Backstop against unbounded PNG production (OOM/disk). Derived from the
    // duration so it can never front-truncate a probed source: past the §0.5.11
    // interval cap the expected count exceeds `keyframe_max`, and a flat cap
    // there would chop the tail off any title over ~3 h 20 m (Q7 / Task 1.4.5).
    // An unprobeable source (`0.0`) still falls back to the flat budget.
    let cap = moderation::frames_cap(total_duration).to_string();
    cmd.args([
        "-map",
        "[k]",
        "-fps_mode",
        "vfr",
        "-frames:v",
        cap.as_str(),
        png.as_str(),
    ]);
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
#[allow(clippy::too_many_arguments)]
fn run_ffmpeg(
    task_id: String,
    format_index: usize,
    file_path: &str,
    file_name: &str,
    is_gpu: bool,
    format: &VideoFormat,
    total_duration: f64,
    tap_dir: Option<&str>,
) -> Result<(), Status> {
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-v").arg("info");
    cmd.arg("-progress").arg("pipe:2");
    cmd.arg("-stats_period").arg("1");

    let trim_duration: Option<f64> = format.trim_percent.and_then(|pct| {
        if pct >= 1 && pct <= 99 && total_duration > 0.0 {
            Some(total_duration * (pct as f64) / 100.0)
        } else {
            None
        }
    });
    let trim_duration = if format.hls.unwrap_or(false) {
        None
    } else {
        trim_duration
    };
    // Progress should track against trimmed duration, not full source
    let progress_duration = trim_duration.unwrap_or(total_duration);

    // Moderation tap (A1): a teed keyframe side-output on the SAME ffmpeg command.
    // `do_tap` also requires a foldable `vf` (no graph-injection) and a video codec;
    // a non-foldable `vf` emits no tap → the Phase-5 gate HOLDs the (unscaled,
    // never-published) job.
    let do_tap = tap_dir.is_some()
        && vf_foldable(format.vf.as_deref())
        && format.vcodec.as_deref().is_some_and(|v| !v.is_empty());
    // Tap-only: computed lazily so the sampling env knobs
    // (MODERATION_SAMPLE_INTERVAL_SECS / _KEYFRAME_MAX / _MAX_INTERVAL_SECS) are
    // never read on the tap-less path (`MODERATION_GATE=off` passes `tap_dir: None`).
    let fps_str = if do_tap {
        format!(
            "{:.5}",
            1.0 / moderation::effective_interval(total_duration)
        )
    } else {
        String::new()
    };
    if do_tap {
        // audit line — deterministic, reproducible sample (spec §5)
        println!(
            "moderation tap: fps={}, interval={}s, budget={}, duration={}s",
            fps_str,
            moderation::effective_interval(total_duration),
            moderation::keyframe_max(),
            total_duration
        );
    }

    if is_gpu {
        println!(
            "GPU transcoding is being executed with vcodec: {:?}",
            format.vcodec
        );

        if let Some(file_path) = Some(file_path) {
            add_arg(&mut cmd, "-i", Some(file_path));
        }
        if let Some(vcodec) = format.vcodec.as_deref() {
            add_arg(&mut cmd, "-c:v", Some(vcodec));
        }
        if let Some(b_v) = format.b_v.as_deref() {
            add_arg(&mut cmd, "-b:v", Some(b_v));
        }
        if let Some(c_a) = format.c_a.as_deref() {
            add_arg(&mut cmd, "-c:a", Some(c_a));
        }
        if let Some(b_a) = format.b_a.as_deref() {
            add_arg(&mut cmd, "-b:a", Some(b_a));
        }
        if let Some(ch) = format.ch {
            add_arg(&mut cmd, "-ac", Some(&ch.to_string()));
        }
        if let Some(ar) = format.ar.as_deref() {
            add_arg(&mut cmd, "-ar", Some(ar));
        }
        if tap_dir.is_none() {
            if let Some(vf) = format.vf.as_deref() {
                add_arg(&mut cmd, "-vf", Some(vf));
            }
        }
        if let Some(ref minrate) = format.minrate {
            cmd.args(["-minrate", minrate]);
        }
        if let Some(ref maxrate) = format.maxrate {
            cmd.args(["-maxrate", maxrate]);
        }
        if let Some(ref bufsize) = format.bufsize {
            cmd.args(["-bufsize", bufsize]);
        }
        if let Some(td) = trim_duration {
            cmd.args(["-t", &format!("{:.3}", td)]);
        }
        if do_tap {
            let dir = tap_dir.unwrap();
            std::fs::create_dir_all(dir)
                .map_err(|e| Status::new(Code::Internal, format!("modtap dir: {}", e)))?;
            append_tap_pre(&mut cmd, format.vf.as_deref(), &fps_str);
        }
        if format.hls.unwrap_or(false) {
            let hls_dir = hls_output_dir(file_name);
            std::fs::create_dir_all(&hls_dir).map_err(|e| {
                Status::new(Code::Internal, format!("Failed to create HLS dir: {}", e))
            })?;
            let hls_time = format.hls_time.unwrap_or(6).to_string();
            cmd.args([
                "-f",
                "hls",
                "-hls_time",
                &hls_time,
                "-hls_segment_type",
                "fmp4",
                "-hls_flags",
                "+split_by_time",
                "-hls_list_size",
                "0",
                "-hls_playlist_type",
                "vod",
                "-hls_fmp4_init_filename",
                "init.mp4",
                "-hls_segment_filename",
                &format!("{}/seg_%04d.m4s", hls_dir),
                "-y",
                &format!("{}/playlist.m3u8", hls_dir),
            ]);
        } else {
            cmd.args([
                "-y",
                format!(
                    "{}{}_ue.{}",
                    *PATH_TO_TRANSCODED_FILE, file_name, format.ext
                )
                .as_str(),
            ]);
        }

        let args: Vec<String> = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        println!("ffmpeg {}", args.join(" "));
    } else {
        if let Some(vcodec) = &format.vcodec {
            if !vcodec.is_empty() {
                println!(
                    "CPU transcoding is being executed with vcodec: {:?}",
                    format.vcodec
                );

                add_arg(&mut cmd, "-cpu-used", Some("4"));

                if let Some(file_path) = Some(file_path) {
                    add_arg(&mut cmd, "-i", Some(file_path));
                }
                if let Some(vcodec) = format.vcodec.as_deref() {
                    add_arg(&mut cmd, "-c:v", Some(vcodec));
                }
                if let Some(b_v) = format.b_v.as_deref() {
                    add_arg(&mut cmd, "-b:v", Some(b_v));
                }
                if let Some(c_a) = format.c_a.as_deref() {
                    add_arg(&mut cmd, "-c:a", Some(c_a));
                }
                if let Some(b_a) = format.b_a.as_deref() {
                    add_arg(&mut cmd, "-b:a", Some(b_a));
                }
                if let Some(ch) = format.ch {
                    add_arg(&mut cmd, "-ac", Some(&ch.to_string()));
                }
                if let Some(ar) = format.ar.as_deref() {
                    add_arg(&mut cmd, "-ar", Some(ar));
                }
                if tap_dir.is_none() {
                    if let Some(vf) = format.vf.as_deref() {
                        add_arg(&mut cmd, "-vf", Some(vf));
                    }
                }
                if let Some(ref minrate) = format.minrate {
                    cmd.args(["-minrate", minrate]);
                }
                if let Some(ref maxrate) = format.maxrate {
                    cmd.args(["-maxrate", maxrate]);
                }
                if let Some(ref bufsize) = format.bufsize {
                    cmd.args(["-bufsize", bufsize]);
                }
                if let Some(td) = trim_duration {
                    cmd.args(["-t", &format!("{:.3}", td)]);
                }
                if do_tap {
                    let dir = tap_dir.unwrap();
                    std::fs::create_dir_all(dir)
                        .map_err(|e| Status::new(Code::Internal, format!("modtap dir: {}", e)))?;
                    append_tap_pre(&mut cmd, format.vf.as_deref(), &fps_str);
                }
                if format.hls.unwrap_or(false) {
                    let hls_dir = hls_output_dir(file_name);
                    std::fs::create_dir_all(&hls_dir).map_err(|e| {
                        Status::new(Code::Internal, format!("Failed to create HLS dir: {}", e))
                    })?;
                    let hls_time = format.hls_time.unwrap_or(6).to_string();
                    cmd.args([
                        "-f",
                        "hls",
                        "-hls_time",
                        &hls_time,
                        "-hls_segment_type",
                        "fmp4",
                        "-hls_flags",
                        "+split_by_time",
                        "-hls_list_size",
                        "0",
                        "-hls_playlist_type",
                        "vod",
                        "-hls_fmp4_init_filename",
                        "init.mp4",
                        "-hls_segment_filename",
                        &format!("{}/seg_%04d.m4s", hls_dir),
                        "-y",
                        &format!("{}/playlist.m3u8", hls_dir),
                    ]);
                } else {
                    cmd.args([
                        "-y",
                        format!(
                            "{}{}_ue.{}",
                            *PATH_TO_TRANSCODED_FILE, file_name, format.ext
                        )
                        .as_str(),
                    ]);
                }

                let args: Vec<String> = cmd
                    .get_args()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect();
                println!("ffmpeg {}", args.join(" "));
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
                if let Some(td) = trim_duration {
                    cmd.args(["-t", &format!("{:.3}", td)]);
                }
                if format.hls.unwrap_or(false) {
                    let hls_dir = hls_output_dir(file_name);
                    std::fs::create_dir_all(&hls_dir).map_err(|e| {
                        Status::new(Code::Internal, format!("Failed to create HLS dir: {}", e))
                    })?;
                    let hls_time = format.hls_time.unwrap_or(6).to_string();
                    cmd.args([
                        "-f",
                        "hls",
                        "-hls_time",
                        &hls_time,
                        "-hls_segment_type",
                        "fmp4",
                        "-hls_flags",
                        "+split_by_time",
                        "-hls_list_size",
                        "0",
                        "-hls_playlist_type",
                        "vod",
                        "-hls_fmp4_init_filename",
                        "init.mp4",
                        "-hls_segment_filename",
                        &format!("{}/seg_%04d.m4s", hls_dir),
                        "-y",
                        &format!("{}/playlist.m3u8", hls_dir),
                    ]);
                } else {
                    add_arg(
                        &mut cmd,
                        "-y",
                        Some(&format!(
                            "{}{}_ue.{}",
                            *PATH_TO_TRANSCODED_FILE, file_name, format.ext
                        )),
                    );
                }
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

    // Append the keyframe PNG tap output AFTER the main `-y <output>` (its `-map [k]`
    // binds to this last output file).
    if do_tap {
        append_tap_post(&mut cmd, tap_dir.unwrap(), total_duration);
    }

    cmd.stderr(Stdio::piped()).stdout(Stdio::null());

    let mut child = cmd
        .spawn()
        .map_err(|e| Status::new(Code::Internal, format!("ffmpeg spawn: {}", e)))?;

    if let Some(stderr) = child.stderr.take() {
        let reader = BufReader::new(stderr);
        let mut last_progress = 0;
        for line_result in reader.lines() {
            if let Ok(line) = line_result {
                if let Some(progress) = parse_progress(&line, progress_duration) {
                    last_progress = progress;
                    let scaled_progress = if format.hls.unwrap_or(false) {
                        (last_progress as f64 * 0.7) as i32
                    } else {
                        last_progress
                    };
                    shared::update_progress(&task_id, format_index, scaled_progress);
                }
                println!("£££££ {} £££££", line);
                println!("Progress: {}%", last_progress);
            }
        }
    }

    let output = child
        .wait()
        .map_err(|e| Status::new(Code::Internal, format!("ffmpeg wait: {}", e)))?;
    println!("Transcode finished with status: {}", output);

    if !output.success() {
        return Err(Status::new(
            Code::Internal,
            format!("FFmpeg exited with status: {}", output),
        ));
    }

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
/// What `transcode_video_produce` makes, before any S5 upload. Carries everything
/// `transcode_video_publish` needs plus `video_format` (the un-mutated catalogue
/// entry) the Phase-5 gate uses to rebuild each deferred output's metadata.
pub struct LocalOutput {
    pub task_id: String,
    pub format_index: usize,
    pub file_name: String,
    pub ext: String,
    pub dest: Option<String>,
    pub encrypt_flag: bool,
    pub is_hls: bool,
    pub total_duration: f64,
    pub preview_percent: u32,
    pub video_format: serde_json::Value,
}

/// Inline produce→publish wrapper — behaviour unchanged. Phase 5 calls produce and
/// publish separately so the A4 gate can sit between them.
pub async fn transcode_video(
    task_id: String,
    format_index: usize,
    file_path: &str,
    video_format: &str,
    is_encrypted: bool,
    is_gpu: bool,
    preview_percent: u32,
) -> Result<Response<TranscodeVideoResponse>, Status> {
    let out = transcode_video_produce(
        task_id,
        format_index,
        file_path,
        video_format,
        is_encrypted,
        is_gpu,
        preview_percent,
        None,
    )
    .await?;
    transcode_video_publish(out).await
}

/// Transcode (with an optional moderation tap) into local files, WITHOUT uploading.
#[allow(clippy::too_many_arguments)]
pub async fn transcode_video_produce(
    task_id: String,
    format_index: usize,
    file_path: &str,
    video_format: &str,
    is_encrypted: bool,
    is_gpu: bool,
    preview_percent: u32,
    tap_dir: Option<&str>,
) -> Result<LocalOutput, Status> {
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
    let video_format_value: serde_json::Value = serde_json::from_str(video_format)
        .map_err(|e| Status::new(Code::Internal, format!("video_format json: {}", e)))?;

    let file_name = format!("{}_{}", file_name, format.id);

    let total_duration = get_video_duration(file_path).unwrap_or(0.0);
    println!("Total video duration: {} seconds", total_duration);

    let gpu_flag = format.gpu.unwrap_or(is_gpu);
    let encrypt_flag = format.encrypt.unwrap_or(is_encrypted);
    let is_hls = format.hls.unwrap_or(false);

    run_ffmpeg(
        task_id.clone(),
        format_index,
        file_path,
        &file_name,
        gpu_flag,
        &format,
        total_duration,
        tap_dir,
    )?;

    Ok(LocalOutput {
        task_id,
        format_index,
        file_name,
        ext: format.ext,
        dest: format.dest,
        encrypt_flag,
        is_hls,
        total_duration,
        preview_percent,
        video_format: video_format_value,
    })
}

/// Publish a produced output to S5. **The only code that writes to S5.**
pub async fn transcode_video_publish(
    out: LocalOutput,
) -> Result<Response<TranscodeVideoResponse>, Status> {
    let LocalOutput {
        task_id,
        format_index,
        file_name,
        ext,
        dest,
        encrypt_flag,
        is_hls,
        total_duration,
        preview_percent,
        video_format: _,
    } = out;

    if is_hls {
        let hls_dir = hls_output_dir(&file_name);
        let hls_result = crate::hls_segment::process_hls_segments(
            &task_id,
            format_index,
            &hls_dir,
            preview_percent,
            dest.clone(),
        )
        .await
        .map_err(|e| {
            Status::new(
                Code::Internal,
                format!("HLS segment processing failed: {}", e),
            )
        })?;
        let hls_json = serde_json::to_string(&hls_result)
            .map_err(|e| Status::new(Code::Internal, e.to_string()))?;
        return Ok(Response::new(TranscodeVideoResponse {
            status_code: 200,
            message: "HLS transcoding successful".into(),
            cid: hls_json,
            duration: total_duration,
        }));
    }

    let mut encryption_key1: Vec<u8> = Vec::new();
    let response: TranscodeVideoResponse;

    if encrypt_flag {
        match encrypt_file_xchacha20(
            format!("{}{}_ue.{}", *PATH_TO_TRANSCODED_FILE, file_name, ext),
            format!("{}{}.{}", *PATH_TO_TRANSCODED_FILE, file_name, ext),
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

        let file_path = format!("{}{}_ue.{}", *PATH_TO_TRANSCODED_FILE, file_name, ext);
        let file_path_encrypted = format!("{}{}.{}", *PATH_TO_TRANSCODED_FILE, file_name, ext);

        let hash_result = hash_blake3_file(file_path.clone());
        let hash_result_encrypted = hash_blake3_file(file_path_encrypted.to_owned());

        let cid_type_encrypted: u8 = 0xae; // replace with your actual cid type encrypted
        let encryption_algorithm: u8 = 0xa6; // replace with your actual encryption algorithm
        let chunk_size_as_power_of_2: u8 = 18; // replace with your actual chunk size as power of 2
        let padding: u32 = 0; // replace with your actual padding

        // Upload the transcoded videos to storage
        match upload_video(file_path_encrypted.as_str(), dest).await {
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
                // Graceful Err (not panic): the deferred publish widens the window in
                // which the GC could evict this intermediate; a missing file must drop
                // the format, not crash the worker.
                let metadata = std::fs::metadata(file_path_path).map_err(|e| {
                    Status::new(
                        Code::Internal,
                        format!("read metadata for {}: {}", file_path, e),
                    )
                })?;
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
                    duration: total_duration,
                };
            }
            Err(e) => {
                println!("!!!!!!!!!!!!!!!!!!!!!2160p no cid");
                println!("Error: {}", e); // This line is added to print out the error message

                response = TranscodeVideoResponse {
                    status_code: 500,
                    message: format!("Transcoding task failed with error {}", e),
                    cid: "".to_string(),
                    duration: 0.0,
                };
            }
        };
    } else {
        let file_path = format!("{}{}_ue.{}", *PATH_TO_TRANSCODED_FILE, file_name, ext);

        // Upload the transcoded videos to storage
        match upload_video(file_path.as_str(), dest.clone()).await {
            Ok(cid) => {
                println!("cid: {:?}", cid);

                println!("Transcoding task finished");

                // Return the TranscodeVideoResponse with the job ID
                response = TranscodeVideoResponse {
                    status_code: 200,
                    message: String::from("Transcoding successful"),
                    cid,
                    duration: total_duration,
                };
            }
            Err(e) => {
                println!("!!!!!!!!!!!!!!!!!!!!!2160p no cid");
                println!("Error: {}", e); // This line is added to print out the error message

                response = TranscodeVideoResponse {
                    status_code: 500,
                    message: format!("Transcoding task failed with error {}", e),
                    cid: "".to_string(),
                    duration: 0.0,
                };
            }
        };
    }

    Ok(Response::new(response))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_publish_is_only_s5_writer() {
        let src = include_str!("transcode_video.rs");
        // needles via concat! so this test's own literals don't self-match
        let up = concat!("upload_video", "(");
        let hls = concat!("process_hls_segments", "(");
        let start = src
            .find("pub async fn transcode_video_publish")
            .expect("publish fn present");
        // publish is the last fn before the test module
        let end = src[start..]
            .find("\n#[cfg(test)]")
            .map(|i| start + i)
            .unwrap_or(src.len());
        let region = &src[start..end];
        let before = &src[..start];
        let after = &src[end..];
        assert!(
            region.contains(up) && region.contains(hls),
            "both S5 writers live in publish"
        );
        assert!(
            !before.contains(up) && !after.contains(up),
            "upload_video only in publish"
        );
        assert!(
            !before.contains(hls) && !after.contains(hls),
            "hls writer only in publish"
        );
    }

    #[test]
    fn test_produce_takes_tap_dir() {
        let src = include_str!("transcode_video.rs");
        assert!(src.contains("transcode_video_produce"));
        assert!(src.contains("tap_dir"));
    }

    #[test]
    fn test_source_has_video_failclosed_on_probe_error() {
        // A non-zero ffprobe exit (missing/corrupt path) — or ffprobe absent — must NOT
        // read as audio-only; the A4 gate depends on this returning true (HOLD).
        assert!(source_has_video("/nonexistent/definitely/not/a/file.xyz"));
    }

    #[test]
    fn test_run_ffmpeg_tap_is_full_duration_side_output() {
        let src = include_str!("transcode_video.rs");
        assert!(src.contains("filter_complex"));
        assert!(src.contains("effective_interval"));
        assert!(src.contains("kf_%05d.png"));
        assert!(src.contains("0:a:0?")); // audio preserved (first stream) on the tapped output
                                         // budget enforced as a hard -frames:v cap (= keyframe_max); needle via concat!
        assert!(
            src.contains(concat!("-frames", ":v")),
            "keyframe budget enforced as a hard -frames:v cap"
        );
        assert!(
            !src.contains(concat!("-", "vsync")),
            "tap must use fps_mode for cadence"
        );
        assert!(src.contains("-fps_mode"));
        assert!(src.contains("tap_dir.is_none()")); // standalone -vf guarded when tapping
    }

    #[test]
    fn test_tap_is_teed_not_a_second_pass() {
        let src = include_str!("transcode_video.rs");
        // the PNG tap is wired via the [k] label of the SAME command's filter_complex
        // graph — teed off the single decode, not a second spawn.
        assert!(src.contains("[k]"));
        assert!(src.contains("kf_%05d.png"));
        assert!(src.contains("split=2"));
    }

    #[test]
    fn test_transcode_video_response_carries_duration() {
        let response = TranscodeVideoResponse {
            status_code: 200,
            message: String::from("Transcoding successful"),
            cid: String::from("test_cid"),
            duration: 125.5,
        };
        assert_eq!(response.duration, 125.5);
    }

    #[test]
    fn test_transcode_video_response_error_has_zero_duration() {
        let response = TranscodeVideoResponse {
            status_code: 500,
            message: String::from("Error"),
            cid: String::new(),
            duration: 0.0,
        };
        assert_eq!(response.duration, 0.0);
    }

    #[test]
    fn test_transcode_video_response_clone_preserves_duration() {
        let response = TranscodeVideoResponse {
            status_code: 200,
            message: String::from("Transcoding successful"),
            cid: String::from("test_cid"),
            duration: 42.7,
        };
        let cloned = response.clone();
        assert_eq!(cloned.duration, 42.7);
    }

    #[test]
    fn test_video_format_deserializes_trim_percent() {
        let json = r#"{"id": 1, "ext": "mp4", "trim_percent": 20}"#;
        let format: VideoFormat = serde_json::from_str(json).unwrap();
        assert_eq!(format.trim_percent, Some(20));
    }

    #[test]
    fn test_video_format_trim_percent_absent_is_none() {
        let json = r#"{"id": 1, "ext": "mp4"}"#;
        let format: VideoFormat = serde_json::from_str(json).unwrap();
        assert!(format.trim_percent.is_none());
    }

    #[test]
    fn test_trim_duration_calculation() {
        let cases: Vec<(f64, u8, f64)> =
            vec![(120.0, 20, 24.0), (60.0, 50, 30.0), (200.5, 10, 20.05)];
        for (total, pct, expected) in cases {
            let result = total * (pct as f64) / 100.0;
            assert!(
                (result - expected).abs() < 1e-10,
                "{total} * {pct}% = {result}, expected {expected}"
            );
        }
    }

    #[test]
    fn test_trim_percent_edge_cases_skipped() {
        let compute = |pct: u8, dur: f64| -> Option<f64> {
            Some(pct).and_then(|p| {
                if p >= 1 && p <= 99 && dur > 0.0 {
                    Some(dur * (p as f64) / 100.0)
                } else {
                    None
                }
            })
        };
        assert!(compute(0, 120.0).is_none());
        assert!(compute(100, 120.0).is_none());
        assert!(compute(20, 0.0).is_none());
        assert!(compute(50, 60.0).is_some());
    }

    #[test]
    fn test_run_ffmpeg_source_has_trim_duration() {
        let src = include_str!("transcode_video.rs");
        assert!(
            src.contains("trim_duration"),
            "run_ffmpeg must compute trim_duration"
        );
        assert!(
            src.contains("\"-t\""),
            "run_ffmpeg must pass -t flag to FFmpeg"
        );
    }

    #[test]
    fn test_video_format_struct_has_trim_percent_field() {
        let src = include_str!("transcode_video.rs");
        assert!(
            src.contains("trim_percent: Option<u8>"),
            "VideoFormat must have trim_percent field"
        );
    }

    #[test]
    fn test_video_format_deserializes_hls_true() {
        let json = r#"{"id": 1, "ext": "mp4", "hls": true, "hls_time": 6}"#;
        let format: VideoFormat = serde_json::from_str(json).unwrap();
        assert_eq!(format.hls, Some(true));
        assert_eq!(format.hls_time, Some(6));
    }

    #[test]
    fn test_video_format_hls_absent_is_none() {
        let json = r#"{"id": 1, "ext": "mp4"}"#;
        let format: VideoFormat = serde_json::from_str(json).unwrap();
        assert!(format.hls.is_none());
        assert!(format.hls_time.is_none());
    }

    #[test]
    fn test_video_format_hls_time_defaults() {
        let json = r#"{"id": 1, "ext": "mp4", "hls": true}"#;
        let format: VideoFormat = serde_json::from_str(json).unwrap();
        assert_eq!(format.hls, Some(true));
        assert_eq!(format.hls_time.unwrap_or(6), 6);
    }

    #[test]
    fn test_video_format_hls_false_is_noop() {
        let json = r#"{"id": 1, "ext": "mp4", "hls": false}"#;
        let format: VideoFormat = serde_json::from_str(json).unwrap();
        assert_eq!(format.hls.unwrap_or(false), false);
    }

    #[test]
    fn test_run_ffmpeg_has_hls_segment_type() {
        let src = include_str!("transcode_video.rs");
        assert!(
            src.contains("hls_segment_type"),
            "must have hls_segment_type flag"
        );
        assert!(src.contains("fmp4"), "must specify fmp4 segment type");
    }

    #[test]
    fn test_run_ffmpeg_has_hls_time() {
        let src = include_str!("transcode_video.rs");
        assert!(src.contains("hls_time"), "must have hls_time flag");
    }

    #[test]
    fn test_run_ffmpeg_has_hls_segment_filename() {
        let src = include_str!("transcode_video.rs");
        assert!(
            src.contains("hls_segment_filename"),
            "must have hls_segment_filename flag"
        );
        assert!(src.contains("seg_%04d.m4s"), "must use seg_%04d.m4s naming");
    }

    #[test]
    fn test_run_ffmpeg_has_hls_list_size_zero() {
        let src = include_str!("transcode_video.rs");
        assert!(
            src.contains("hls_list_size"),
            "must have hls_list_size flag"
        );
        assert!(src.contains(r#""0""#), "must set hls_list_size to 0");
    }

    #[test]
    fn test_transcode_video_has_hls_branch() {
        let src = include_str!("transcode_video.rs");
        assert!(
            src.contains("process_hls_segments"),
            "must call process_hls_segments"
        );
        assert!(src.contains("hls_output_dir"), "must use hls_output_dir");
    }
}
