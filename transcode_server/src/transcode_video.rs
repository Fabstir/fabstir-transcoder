use crate::moderation;
use crate::shared::{self, LocalFile};

use crate::encrypt_file::encrypt_file_xchacha20;
use crate::encrypted_cid::create_encrypted_cid;
use crate::s5::hash_blake3_file;
use crate::s5::upload_video;
use crate::utils::{bytes_to_base64url, hash_bytes_to_cid};
use dotenv::var;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use tokio::io::AsyncBufReadExt;
use tonic::{Code, Response, Status};

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

/// True for any NVENC encoder (`h264_nvenc`, `hevc_nvenc`, `av1_nvenc`, ...).
fn is_nvenc(vcodec: &str) -> bool {
    vcodec.ends_with("_nvenc")
}

/// Emit `-c:v <vcodec>`, plus `-bf 0` on NVENC.
///
/// **Host workaround, not a preference (2026-07-31).** On the L40S production
/// host every NVENC encode — h264, hevc and av1 alike, across drivers 580 and
/// 610, four FFmpeg builds, inside and outside Docker — fails with
/// "Failed locking bitstream buffer: invalid param (8)" as soon as B-frame
/// reordered output engages. The failure point moves with resolution, which
/// makes it masquerade as a resolution limit. Disabling B-frames fixes it
/// completely (clean 600-frame runs at 720p and 1080p).
///
/// Cost is a few percent of bitrate efficiency and nothing else. The
/// underlying platform issue (likely the hypervisor's handling of the
/// reordered-output path) is being chased separately; **lift this once that
/// lands** — it is deliberately one function so there is a single place to
/// remove, rather than two drifting call sites.
fn add_video_codec(cmd: &mut Command, vcodec: &str) {
    add_arg(cmd, "-c:v", Some(vcodec));
    if is_nvenc(vcodec) {
        cmd.args(["-bf", "0"]);
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

/// Runs one of the short ffprobe calls, which are never meant to take long.
const PROBE_TIMEOUT: Duration = Duration::from_secs(300);

/// How long to wait for a child to go after killing it. One stuck inside a driver
/// can't even be killed, and the queue has to move on regardless.
const KILL_WAIT: Duration = Duration::from_secs(30);

const DEFAULT_STALL_LIMIT: Duration = Duration::from_secs(15 * 60);

async fn run_probe(cmd: Command, limit: Duration) -> Result<std::process::Output, String> {
    let mut cmd = tokio::process::Command::from(cmd);
    cmd.stdin(Stdio::null()).kill_on_drop(true);
    match tokio::time::timeout(limit, cmd.output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(e)) => Err(format!("could not run: {}", e)),
        Err(_) => Err(format!("timed out after {:?}", limit)),
    }
}

fn parse_duration(stdout: &[u8]) -> Result<f64, String> {
    let duration: f64 = String::from_utf8_lossy(stdout)
        .trim()
        .parse()
        .map_err(|e: std::num::ParseFloatError| e.to_string())?;
    if duration.is_finite() && duration >= 0.0 {
        Ok(duration)
    } else {
        Err(format!("unusable duration: {}", duration))
    }
}

/// Gets video duration in seconds using `ffprobe`.
///
/// # Arguments
/// * `file_path`: Path to the video file.
///
/// # Returns:
/// `Result<f64, String>` - Duration in seconds or error message.
///
pub async fn get_video_duration(file_path: &str) -> Result<f64, String> {
    let mut cmd = Command::new("ffprobe");
    cmd.args([
        "-v",
        "error",
        "-show_entries",
        "format=duration",
        "-of",
        "default=noprint_wrappers=1:nokey=1",
        file_path,
    ]);
    let output = run_probe(cmd, PROBE_TIMEOUT).await?;

    if output.status.success() {
        parse_duration(&output.stdout)
    } else {
        Err(String::from("Failed to retrieve video duration"))
    }
}

/// Reads FFmpeg's media clock from a line of its output, in seconds: `out_time=` from
/// `-progress`, or `time=` from a status report.
fn parse_media_time(line: &str) -> Option<f64> {
    static TIME: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"time=(\d+):(\d+):(\d+\.\d+)").expect("a valid regex"));
    let caps = TIME.captures(line)?;
    let number = |i: usize| caps.get(i)?.as_str().parse::<f64>().ok();
    Some(number(1)? * 3600.0 + number(2)? * 60.0 + number(3)?)
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
    let current_time_seconds = parse_media_time(line)?;
    Some(((current_time_seconds / total_duration) * 100.0).round() as i32)
}

/// The `key=value` lines `-progress` and the status reports print every second or
/// so. Worth reading for the clock, but too many to log for hours on end.
fn is_progress_line(line: &str) -> bool {
    static PROGRESS: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"^\s*[a-z0-9_]+=").expect("a valid regex"));
    PROGRESS.is_match(line)
}

/// `FFMPEG_STALL_TIMEOUT_SECS`, where 0 turns the watchdog off.
fn stall_limit_from(setting: Option<&str>) -> Option<Duration> {
    match setting.map(|value| value.trim().parse::<u64>()) {
        Some(Ok(0)) => None,
        Some(Ok(seconds)) => Some(Duration::from_secs(seconds)),
        Some(Err(_)) => {
            eprintln!("FFMPEG_STALL_TIMEOUT_SECS is not a number; using the default");
            Some(DEFAULT_STALL_LIMIT)
        }
        None => Some(DEFAULT_STALL_LIMIT),
    }
}

fn ffmpeg_stall_limit() -> Option<Duration> {
    stall_limit_from(var("FFMPEG_STALL_TIMEOUT_SECS").ok().as_deref())
}

/// Decides whether a child has stopped getting anywhere. Only its media clock moving
/// forward counts: FFmpeg 7 and later keep printing status reports while stuck.
struct StallWatch {
    limit: Option<Duration>,
    latest_time: f64,
    last_advance: Instant,
}

impl StallWatch {
    fn new(limit: Option<Duration>, now: Instant) -> Self {
        StallWatch {
            limit,
            latest_time: f64::NEG_INFINITY,
            last_advance: now,
        }
    }

    fn observe(&mut self, media_time: f64, now: Instant) {
        if media_time > self.latest_time {
            self.latest_time = media_time;
            self.last_advance = now;
        }
    }

    fn deadline(&self) -> Option<Instant> {
        self.limit.map(|limit| self.last_advance + limit)
    }
}

/// Waits for something the child is doing, up to the watchdog's deadline. `None`
/// means the deadline came first.
async fn until_deadline<F: std::future::Future>(
    deadline: Option<Instant>,
    work: F,
) -> Option<F::Output> {
    match deadline {
        Some(at) => tokio::time::timeout_at(at.into(), work).await.ok(),
        None => Some(work.await),
    }
}

async fn kill_stalled_child(mut child: tokio::process::Child, limit: Option<Duration>) -> String {
    let _ = child.start_kill();
    let _ = tokio::time::timeout(KILL_WAIT, child.wait()).await;
    format!(
        "FFmpeg stalled: no progress for {:?}, killed it",
        limit.unwrap_or_default()
    )
}

/// Runs FFmpeg without tying up an async worker, passing each line of its output to
/// `on_line`. Kills it if it stops making progress for `stall_limit`, or if this
/// future is dropped, which is how a job past its deadline gets stopped.
async fn run_ffmpeg_child(
    cmd: Command,
    stall_limit: Option<Duration>,
    mut on_line: impl FnMut(&str),
) -> Result<ExitStatus, String> {
    let mut cmd = tokio::process::Command::from(cmd);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| format!("ffmpeg spawn: {}", e))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "ffmpeg has no stderr".to_string())?;
    let mut reader = tokio::io::BufReader::new(stderr);
    let mut watch = StallWatch::new(stall_limit, Instant::now());

    // Bytes rather than text, because what FFmpeg prints from a file's metadata can
    // be anything. Kept across waits: a partial line stays here until the rest comes.
    let mut line = Vec::new();
    loop {
        match until_deadline(watch.deadline(), reader.read_until(b'\n', &mut line)).await {
            None => return Err(kill_stalled_child(child, stall_limit).await),
            Some(Ok(0)) => break,
            Some(Ok(_)) => {
                let text = String::from_utf8_lossy(&line);
                if let Some(media_time) = parse_media_time(&text) {
                    watch.observe(media_time, Instant::now());
                }
                on_line(text.trim_end());
                line.clear();
            }
            Some(Err(e)) => {
                let _ = child.start_kill();
                return Err(format!("reading ffmpeg output: {}", e));
            }
        }
    }

    // Its output has ended, but it can still be stuck on the way out.
    match until_deadline(watch.deadline(), child.wait()).await {
        None => Err(kill_stalled_child(child, stall_limit).await),
        Some(Ok(status)) => Ok(status),
        Some(Err(e)) => Err(format!("ffmpeg wait: {}", e)),
    }
}

pub fn hls_output_dir(file_name: &str) -> String {
    format!("{}{}_hls", *PATH_TO_TRANSCODED_FILE, file_name)
}

/// Where FFmpeg writes a single-file output.
fn plain_output_path(file_name: &str, ext: &str) -> String {
    format!("{}{}_ue.{}", *PATH_TO_TRANSCODED_FILE, file_name, ext)
}

/// What a produced output occupies on disk, however it is stored.
pub fn local_output_path(file_name: &str, ext: &str, is_hls: bool) -> String {
    if is_hls {
        hls_output_dir(file_name)
    } else {
        plain_output_path(file_name, ext)
    }
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
pub async fn source_has_video(path: &str) -> bool {
    let mut cmd = Command::new("ffprobe");
    cmd.args([
        "-v",
        "error",
        "-select_streams",
        "v",
        "-show_entries",
        "stream=codec_type",
        "-of",
        "csv=p=0",
        path,
    ]);
    match run_probe(cmd, PROBE_TIMEOUT).await {
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
pub async fn tap_source_keyframes(file_path: &str, tap_dir: &str) -> Result<(), Status> {
    std::fs::create_dir_all(tap_dir)
        .map_err(|e| Status::new(Code::Internal, format!("modtap dir: {}", e)))?;
    let d = get_video_duration(file_path).await.unwrap_or(0.0);
    let fps = format!(
        "fps={:.5},{}",
        1.0 / moderation::effective_interval(d),
        TAP_SCALE
    );
    let out = format!("{}/kf_%05d.png", tap_dir);
    let cap = moderation::frames_cap(d).to_string(); // duration-derived (see append_tap_post)
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-nostdin",
        "-v",
        "error",
        "-progress",
        "pipe:2",
        "-stats_period",
        "1",
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
    ]);
    let status = run_ffmpeg_child(cmd, ffmpeg_stall_limit(), |_| {})
        .await
        .map_err(|e| Status::new(Code::Internal, format!("source tap: {}", e)))?;
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

/// The video filters a request may ask for. Everything else is refused, because other
/// filters can read files from this machine (`movie`, `subtitles`, `drawtext`, ...) and
/// put them in an output, where the moderation tap, which only reads the source,
/// would never see them.
const ALLOWED_VF_FILTERS: &[&str] = &[
    "scale",
    "scale_cuda",
    "scale_npp",
    "hwupload_cuda",
    "hwdownload",
    "format",
    "fps",
    "crop",
    "pad",
    "setsar",
    "setdar",
    "transpose",
    "hflip",
    "vflip",
    "yadif",
    "bwdif",
];

/// True if `vf` is a plain chain of allowed filters. A comma inside quotes belongs
/// to an expression; quotes and commas are the only syntax allowed, so what gets
/// checked here is what FFmpeg will see.
fn vf_allowed(vf: &str) -> bool {
    if vf.contains(['[', ']', ';', '\\']) {
        return false;
    }
    let mut filters = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    for (i, c) in vf.char_indices() {
        match c {
            '\'' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                filters.push(&vf[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    filters.push(&vf[start..]);
    !in_quotes
        && filters.iter().all(|filter| {
            let name = filter.split('=').next().unwrap_or_default().trim();
            ALLOWED_VF_FILTERS.contains(&name)
        })
}

/// Checks the parts of a format that end up in file names and filter graphs. Both
/// come straight from the request.
pub fn validate_format(format: &VideoFormat) -> Result<(), String> {
    if format.ext.is_empty() || !format.ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(format!("unusable file extension: {:?}", format.ext));
    }
    if let Some(vf) = format.vf.as_deref() {
        if !vf_allowed(vf) {
            return Err(format!("video filter not allowed: {:?}", vf));
        }
    }
    Ok(())
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

/// `-t` for a format that keeps only the start of the video.
fn trim_duration(format: &VideoFormat, total_duration: f64) -> Option<f64> {
    if format.hls.unwrap_or(false) {
        return None;
    }
    format.trim_percent.and_then(|pct| {
        if (1..=99).contains(&pct) && total_duration > 0.0 {
            Some(total_duration * (pct as f64) / 100.0)
        } else {
            None
        }
    })
}

/// How long an output should be if its source's header is telling the truth.
fn expected_output_length(format: &VideoFormat, source_claims: f64) -> f64 {
    trim_duration(format, source_claims).unwrap_or(source_claims)
}

/// True if an output's real length is further from what its source claims than
/// rounding and container differences explain: 1 s, or 1% of a long title.
fn duration_mismatch(expected: f64, measured: f64) -> bool {
    expected > 0.0 && (expected - measured).abs() > (expected * 0.01).max(1.0)
}

/// How long a finished output really is, taken from the output itself: FFmpeg wrote
/// it, so what the source's header claims has no say. FFmpeg's clock would be simpler,
/// but it stops at the start of the last frame (half a second short at 2 fps), and
/// with the moderation tap running it follows the tap past the end of a trimmed output.
async fn measure_output(file_name: &str, ext: &str, is_hls: bool) -> Result<f64, String> {
    if is_hls {
        let playlist = format!("{}/playlist.m3u8", hls_output_dir(file_name));
        let parsed =
            crate::hls_segment::parse_hls_playlist(&playlist).map_err(|e| e.to_string())?;
        Ok(parsed.segments.iter().map(|(_, duration)| duration).sum())
    } else {
        get_video_duration(&plain_output_path(file_name, ext)).await
    }
}

/// True for encoders with a `-cpu-used` option. Others refuse to start if given it.
fn has_cpu_used_option(vcodec: &str) -> bool {
    vcodec == "libaom-av1" || vcodec.starts_with("libvpx")
}

/// Append the output: an HLS playlist with its segments, or a single file. MUST come
/// after all the options that apply to it.
fn add_output(cmd: &mut Command, file_name: &str, format: &VideoFormat) -> Result<(), Status> {
    if format.hls.unwrap_or(false) {
        let hls_dir = hls_output_dir(file_name);
        std::fs::create_dir_all(&hls_dir)
            .map_err(|e| Status::new(Code::Internal, format!("Failed to create HLS dir: {}", e)))?;
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
        cmd.args(["-y", plain_output_path(file_name, &format.ext).as_str()]);
    }
    Ok(())
}

/// Builds the ffmpeg command to transcode a video file based on the specified parameters.
///
/// # Arguments
/// * `file_path` - The path to the input video file to be transcoded.
/// * `file_name` - The name its outputs are based on.
/// * `is_gpu` - Whether this format runs on the GPU. The encoder itself comes from `format`.
/// * `format` - The desired output video format.
/// * `total_duration` - The total duration of the video file in seconds.
/// * `tap_dir` - Where the moderation tap puts its frames, if this format owns the tap.
/// * `pin_streams` - The gate is armed, so outputs must use the stream the tap reads.
///
fn build_ffmpeg_command(
    file_path: &str,
    file_name: &str,
    is_gpu: bool,
    format: &VideoFormat,
    total_duration: f64,
    tap_dir: Option<&str>,
    pin_streams: bool,
) -> Result<Command, Status> {
    let vcodec = format.vcodec.as_deref().filter(|codec| !codec.is_empty());
    let acodec = format.acodec.as_deref().filter(|codec| !codec.is_empty());
    let audio_only = vcodec.is_none() && acodec.is_some();

    // On the GPU path, naming no codec leaves the choice to FFmpeg.
    if !is_gpu && vcodec.is_none() && acodec.is_none() {
        return Err(Status::new(Code::InvalidArgument, "No codec specified"));
    }

    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-nostdin");
    cmd.arg("-v").arg("info");
    cmd.arg("-progress").arg("pipe:2");
    cmd.arg("-stats_period").arg("1");

    // Moderation tap (A1): a teed keyframe side-output on the SAME ffmpeg command.
    // `do_tap` also requires a foldable `vf` (no graph-injection) and a video codec;
    // a non-foldable `vf` emits no tap → the Phase-5 gate HOLDs the (unscaled,
    // never-published) job.
    let tap_output = tap_dir.filter(|_| vf_foldable(format.vf.as_deref()) && vcodec.is_some());
    let do_tap = tap_output.is_some();
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

    println!(
        "{} transcoding is being executed with vcodec: {:?}",
        if is_gpu { "GPU" } else { "CPU" },
        format.vcodec
    );

    add_arg(&mut cmd, "-i", Some(file_path));
    if let Some(vcodec) = vcodec {
        add_video_codec(&mut cmd, vcodec);
        if !is_gpu && has_cpu_used_option(vcodec) {
            add_arg(&mut cmd, "-cpu-used", Some("4"));
        }
    }
    if audio_only {
        // Without this FFmpeg can encode the video too, for a container with a place
        // for cover art: a whole video decode, and a surprise in the output.
        cmd.arg("-vn");
        add_arg(&mut cmd, "-acodec", acodec);
    }
    add_arg(&mut cmd, "-b:v", format.b_v.as_deref());
    add_arg(&mut cmd, "-c:a", format.c_a.as_deref());
    add_arg(&mut cmd, "-b:a", format.b_a.as_deref());
    if let Some(ch) = format.ch {
        add_arg(&mut cmd, "-ac", Some(&ch.to_string()));
    }
    add_arg(&mut cmd, "-ar", format.ar.as_deref());
    if audio_only {
        if let Some(compression_level) = format.compression_level {
            add_arg(
                &mut cmd,
                "-compression_level",
                Some(&compression_level.to_string()),
            );
        }
    }
    // Suppress `-vf` ONLY when the tap actually folds it into the
    // filtergraph. Keying this on `tap_dir.is_none()` was wrong:
    // `do_tap` additionally requires a foldable vf and a video
    // codec, so a format handed a tap_dir but rejected by either
    // check lost its `-vf` entirely and silently produced an
    // unscaled rendition.
    if !do_tap && !audio_only {
        add_arg(&mut cmd, "-vf", format.vf.as_deref());
    }
    if !audio_only {
        add_arg(&mut cmd, "-minrate", format.minrate.as_deref());
        add_arg(&mut cmd, "-maxrate", format.maxrate.as_deref());
        add_arg(&mut cmd, "-bufsize", format.bufsize.as_deref());
    }
    if let Some(td) = trim_duration(format, total_duration) {
        cmd.args(["-t", &format!("{:.3}", td)]);
    }
    if let Some(dir) = tap_output {
        std::fs::create_dir_all(dir)
            .map_err(|e| Status::new(Code::Internal, format!("modtap dir: {}", e)))?;
        append_tap_pre(&mut cmd, format.vf.as_deref(), &fps_str);
    } else if pin_streams && vcodec.is_some() {
        // The tap reads the first video stream, so every published video has to as
        // well. Left to itself FFmpeg takes the biggest, which may not be the first.
        // Optional, as a source can be audio only.
        cmd.args(["-map", "0:v:0?", "-map", "0:a:0?"]);
    }
    add_output(&mut cmd, file_name, format)?;

    // Append the keyframe PNG tap output AFTER the main `-y <output>` (its `-map [k]`
    // binds to this last output file).
    if let Some(dir) = tap_output {
        append_tap_post(&mut cmd, dir, total_duration);
    }

    Ok(cmd)
}

/// Runs ffmpeg for one format, keeping the job's progress up to date as it goes.
///
/// # Arguments
/// * `task_id` - A unique identifier for the transcoding task.
/// * `format_index` - The index specifying the target video format from a predefined list.
/// * The rest as in `build_ffmpeg_command`.
///
/// # Returns
/// A `Result<(), Status>` indicating the success or failure of the transcoding operation.
///
#[allow(clippy::too_many_arguments)]
async fn run_ffmpeg(
    task_id: &str,
    format_index: usize,
    file_path: &str,
    file_name: &str,
    is_gpu: bool,
    format: &VideoFormat,
    total_duration: f64,
    tap_dir: Option<&str>,
    pin_streams: bool,
) -> Result<(), Status> {
    let cmd = build_ffmpeg_command(
        file_path,
        file_name,
        is_gpu,
        format,
        total_duration,
        tap_dir,
        pin_streams,
    )?;

    // Log the exact argv before spawning. The tap builds a two-output
    // filter_complex command, and when one of those graphs fails to configure
    // FFmpeg's error names a filter pad rather than anything traceable back to
    // a format id — which is otherwise only diagnosable by bisecting on the
    // host. Cheap, once per ffmpeg run, and never carries a secret: encrypted
    // sources are cached under a hash of their CID (`source_file_path`).
    println!(
        "ffmpeg argv (format {}): {:?} {:?}",
        format.id,
        cmd.get_program(),
        cmd.get_args().collect::<Vec<_>>()
    );

    // Progress should track against trimmed duration, not full source
    let progress_duration = trim_duration(format, total_duration).unwrap_or(total_duration);
    let is_hls = format.hls.unwrap_or(false);
    let mut next_progress_log = 0;
    let output = run_ffmpeg_child(cmd, ffmpeg_stall_limit(), |line| {
        if let Some(progress) = parse_progress(line, progress_duration) {
            let scaled_progress = if is_hls {
                (progress as f64 * 0.7) as i32
            } else {
                progress
            };
            shared::update_progress(task_id, format_index, scaled_progress);
            if progress >= next_progress_log {
                println!("Progress: format {} at {}%", format.id, progress);
                next_progress_log = progress + 10;
            }
        }
        if !is_progress_line(line) {
            println!("ffmpeg: {}", line);
        }
    })
    .await
    .map_err(|e| Status::new(Code::Internal, e))?;
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
    /// How long the output really is, measured from the file itself.
    pub encoded_duration: f64,
    pub preview_percent: u32,
    pub video_format: serde_json::Value,
    /// Protects the output from GC while it's needed, and deletes it after.
    pub local_file: LocalFile,
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
        false,
    )
    .await?;
    transcode_video_publish(out).await
}

/// Transcode (with an optional moderation tap) into local files, WITHOUT uploading.
///
/// Outputs are named after the task and the format's place in it. That keeps jobs
/// apart when they run on one source at the same time (with one shared name, each
/// could overwrite, or on a hold delete, what the other was still using), and it
/// allows two formats in one job to share an id.
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
    pin_streams: bool,
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

    let file_name = format!("{}_{}_{}_{}", file_name, format.id, task_id, format_index);

    let total_duration = get_video_duration(file_path).await.unwrap_or(0.0);
    println!("Total video duration: {} seconds", total_duration);

    let gpu_flag = format.gpu.unwrap_or(is_gpu);
    let encrypt_flag = format.encrypt.unwrap_or(is_encrypted);
    let is_hls = format.hls.unwrap_or(false);

    // Held from before FFmpeg starts: GC could otherwise take the file while it's
    // being written, and if this fails what's been written so far is removed.
    let output_path = local_output_path(&file_name, &format.ext, is_hls);
    let local_file = if is_hls {
        LocalFile::dir(&output_path)
    } else {
        LocalFile::file(&output_path)
    };

    run_ffmpeg(
        &task_id,
        format_index,
        file_path,
        &file_name,
        gpu_flag,
        &format,
        total_duration,
        tap_dir,
        pin_streams,
    )
    .await?;

    let expected_duration = expected_output_length(&format, total_duration);
    let encoded_duration = match measure_output(&file_name, &format.ext, is_hls).await {
        Ok(measured) => {
            if duration_mismatch(expected_duration, measured) {
                eprintln!(
                    "DURATION MISMATCH task_id={} format={} source_claims={:.3}s encoded={:.3}s",
                    task_id, format.id, expected_duration, measured
                );
            }
            measured
        }
        Err(e) => {
            eprintln!(
                "Could not measure format {}: {}. Using what the source claims.",
                format.id, e
            );
            expected_duration
        }
    };

    Ok(LocalOutput {
        task_id,
        format_index,
        file_name,
        ext: format.ext,
        dest: format.dest,
        encrypt_flag,
        is_hls,
        encoded_duration,
        preview_percent,
        video_format: video_format_value,
        local_file,
    })
}

/// Publish a produced output to S5. **The only code that writes to S5.** The local
/// copy goes when this returns, however it went: names are per task, so nothing will
/// ever use it again, and until then it's a decrypted copy on disk.
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
        encoded_duration,
        preview_percent,
        video_format: _,
        local_file: _local_file,
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
            duration: encoded_duration,
        }));
    }

    let file_path = plain_output_path(&file_name, &ext);
    let response = if encrypt_flag {
        let encrypted_file = LocalFile::file(&format!(
            "{}{}.{}",
            *PATH_TO_TRANSCODED_FILE, file_name, ext
        ));
        let encrypted = encrypt_and_hash(file_path, encrypted_file.path().to_string()).await?;

        // Upload the transcoded videos to storage
        match upload_video(encrypted_file.path(), dest).await {
            Ok(_) => {
                println!("Transcoding task finished");
                TranscodeVideoResponse {
                    status_code: 200,
                    message: String::from("Transcoding successful"),
                    cid: encrypted.into_cid(),
                    duration: encoded_duration,
                }
            }
            Err(e) => failed_upload_response(e),
        }
    } else {
        // Upload the transcoded videos to storage
        match upload_video(file_path.as_str(), dest.clone()).await {
            Ok(cid) => {
                println!("Transcoding task finished");
                TranscodeVideoResponse {
                    status_code: 200,
                    message: String::from("Transcoding successful"),
                    cid,
                    duration: encoded_duration,
                }
            }
            Err(e) => failed_upload_response(e),
        }
    };

    Ok(Response::new(response))
}

fn failed_upload_response(error: anyhow::Error) -> TranscodeVideoResponse {
    eprintln!("Upload failed: {}", error);
    TranscodeVideoResponse {
        status_code: 500,
        message: format!("Transcoding task failed with error {}", error),
        cid: "".to_string(),
        duration: 0.0,
    }
}

/// What the CID of an encrypted output is made from. Holds the key, so isn't `Debug`.
struct EncryptedOutput {
    key: Vec<u8>,
    hash: blake3::Hash,
    encrypted_hash: blake3::Hash,
    file_size: u64,
}

impl EncryptedOutput {
    fn into_cid(self) -> String {
        const CID_TYPE_ENCRYPTED: u8 = 0xae;
        const ENCRYPTION_ALGORITHM: u8 = 0xa6;
        const CHUNK_SIZE_AS_POWER_OF_2: u8 = 18;
        const PADDING: u32 = 0;

        let mut encrypted_blob_hash = vec![0x1f];
        encrypted_blob_hash.extend(self.encrypted_hash.as_bytes());

        let cid = hash_bytes_to_cid(self.hash.as_bytes().to_vec(), self.file_size);

        let encrypted_cid_bytes = create_encrypted_cid(
            CID_TYPE_ENCRYPTED,
            ENCRYPTION_ALGORITHM,
            CHUNK_SIZE_AS_POWER_OF_2,
            encrypted_blob_hash,
            self.key,
            PADDING,
            cid,
        );
        format!("u{}", bytes_to_base64url(&encrypted_cid_bytes))
    }
}

/// Encrypts an output and hashes both copies, off the async workers because the files
/// can be many gigabytes. Any failure stops here: carrying on would upload whatever
/// was on disk and publish a CID with no usable key in it.
async fn encrypt_and_hash(plain: String, encrypted: String) -> Result<EncryptedOutput, Status> {
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<EncryptedOutput> {
        let key = encrypt_file_xchacha20(plain.clone(), encrypted.clone(), 0)?;
        Ok(EncryptedOutput {
            key,
            hash: hash_blake3_file(plain.clone())?,
            encrypted_hash: hash_blake3_file(encrypted)?,
            file_size: std::fs::metadata(&plain)?.len(),
        })
    })
    .await;
    match result {
        Ok(Ok(encrypted)) => Ok(encrypted),
        Ok(Err(e)) => Err(Status::new(
            Code::Internal,
            format!("Encryption failed: {}", e),
        )),
        Err(e) => Err(Status::new(
            Code::Internal,
            format!("Encryption task failed: {}", e),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Watching FFmpeg: a stuck child must never stop other jobs ───────────

    fn shell(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]);
        cmd
    }

    const SHORT: Duration = Duration::from_millis(300);

    #[test]
    fn test_parse_media_time_reads_ffmpeg_clocks() {
        for (line, expected) in [
            ("out_time=00:01:02.500000", Some(62.5)),
            (
                "frame=  10 fps=0.0 q=28.0 size=1kB time=01:00:00.00 speed=1x",
                Some(3600.0),
            ),
            ("out_time=N/A", None),
            ("out_time=-00:00:00.023000", None),
            ("progress=continue", None),
        ] {
            assert_eq!(parse_media_time(line), expected, "{}", line);
        }
    }

    #[test]
    fn test_stall_watch_is_only_satisfied_by_the_clock_moving_forward() {
        let start = Instant::now();
        let limit = Duration::from_secs(10);
        let mut watch = StallWatch::new(Some(limit), start);
        assert_eq!(watch.deadline(), Some(start + limit));

        let later = start + Duration::from_secs(5);
        watch.observe(1.0, later);
        assert_eq!(watch.deadline(), Some(later + limit), "the clock moved");

        watch.observe(1.0, later + Duration::from_secs(5));
        assert_eq!(watch.deadline(), Some(later + limit), "the same time again");

        watch.observe(0.5, later + Duration::from_secs(5));
        assert_eq!(watch.deadline(), Some(later + limit), "going backwards");

        assert_eq!(StallWatch::new(None, start).deadline(), None);
    }

    #[test]
    fn test_stall_limit_comes_from_the_environment_and_zero_turns_it_off() {
        assert_eq!(stall_limit_from(None), Some(Duration::from_secs(900)));
        assert_eq!(stall_limit_from(Some("60")), Some(Duration::from_secs(60)));
        assert_eq!(stall_limit_from(Some("0")), None);
        assert_eq!(
            stall_limit_from(Some("soon")),
            Some(Duration::from_secs(900))
        );
    }

    #[tokio::test]
    async fn test_a_child_that_finishes_is_reported_with_its_output() {
        let mut lines = Vec::new();
        let status = run_ffmpeg_child(
            shell("echo first >&2; echo second >&2; exit 3"),
            Some(SHORT),
            |line| lines.push(line.to_string()),
        )
        .await
        .unwrap();
        assert_eq!(status.code(), Some(3));
        assert_eq!(lines, ["first", "second"]);
    }

    /// The hang that used to stop the queue: no output at all, for ever.
    #[tokio::test]
    async fn test_a_silent_child_is_killed_as_stalled() {
        let started = Instant::now();
        let error = run_ffmpeg_child(shell("exec sleep 30"), Some(SHORT), |_| {})
            .await
            .unwrap_err();
        assert!(error.contains("stalled"), "{}", error);
        assert!(
            !error.contains("Some"),
            "written for people to read: {}",
            error
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// FFmpeg 7 and later keep printing status reports while stuck, so output alone
    /// says nothing. Only the media clock counts.
    #[tokio::test]
    async fn test_a_child_printing_without_making_progress_is_killed_as_stalled() {
        let error = run_ffmpeg_child(
            shell("while true; do echo frame=1; echo out_time=00:00:01.000000; sleep 0.02; done"),
            Some(SHORT),
            |_| {},
        )
        .await
        .unwrap_err();
        assert!(error.contains("stalled"), "{}", error);
    }

    #[tokio::test]
    async fn test_a_slow_child_making_progress_is_left_alone() {
        // Runs for about 2.5 limits, moving its clock well within each.
        let script = "for i in 1 2 3 4 5 6 7 8; do \
                      echo out_time=00:00:0$i.000000 >&2; sleep 0.1; done";
        let status = run_ffmpeg_child(shell(script), Some(SHORT), |_| {})
            .await
            .unwrap();
        assert!(status.success());
    }

    /// A child that closes its output and stays alive would otherwise never end.
    #[tokio::test]
    async fn test_a_child_that_closes_its_output_and_hangs_is_killed() {
        let error = run_ffmpeg_child(shell("exec 2>&-; exec sleep 30"), Some(SHORT), |_| {})
            .await
            .unwrap_err();
        assert!(error.contains("stalled"), "{}", error);
    }

    /// Not valid UTF-8: subtitle and metadata text can be anything.
    #[tokio::test]
    async fn test_a_child_printing_bytes_that_are_not_text_is_read_to_the_end() {
        let mut lines = Vec::new();
        let status = run_ffmpeg_child(
            shell(r"printf '\377\376bad\n' >&2; echo out_time=00:00:01.000000 >&2"),
            Some(SHORT),
            |line| lines.push(line.to_string()),
        )
        .await
        .unwrap();
        assert!(status.success());
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1], "out_time=00:00:01.000000");
    }

    /// The inherited stdin could stop FFmpeg waiting on a terminal that never answers.
    #[tokio::test]
    async fn test_a_child_reading_stdin_sees_it_closed() {
        let status = run_ffmpeg_child(shell("cat; exit 0"), Some(SHORT), |_| {})
            .await
            .unwrap();
        assert!(status.success());
    }

    /// This test's runtime has one thread. If the child were waited on with blocking
    /// calls, the ticker below couldn't run at all until it finished.
    #[tokio::test]
    async fn test_waiting_on_a_child_leaves_the_async_worker_free() {
        let ticks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = ticks.clone();
        tokio::spawn(async move {
            loop {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        tokio::task::yield_now().await;
        let status = run_ffmpeg_child(shell("sleep 0.3"), None, |_| {})
            .await
            .unwrap();
        assert!(status.success());
        let ticks = ticks.load(std::sync::atomic::Ordering::SeqCst);
        assert!(ticks >= 10, "the worker was blocked: only {} ticks", ticks);
    }

    /// A job that passes its deadline is stopped by dropping it: its FFmpeg must die
    /// with it, or the GPU session and the output file stay in use for ever.
    #[tokio::test]
    async fn test_stopping_the_wait_kills_the_child() {
        let pid_file = std::env::temp_dir().join(format!("pid-{}", uuid::Uuid::new_v4()));
        let script = format!("echo $$ > {}; exec sleep 30", pid_file.display());
        let wait = run_ffmpeg_child(shell(&script), None, |_| {});
        assert!(tokio::time::timeout(Duration::from_millis(300), wait)
            .await
            .is_err());

        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let _ = std::fs::remove_file(&pid_file);
        let proc_dir = format!("/proc/{}/stat", pid.trim());
        for _ in 0..200 {
            // A zombie has been killed, and is only waiting to be collected.
            match std::fs::read_to_string(&proc_dir) {
                Err(_) => return,
                Ok(stat) if stat.contains(") Z ") => return,
                Ok(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
        panic!("the child is still running");
    }

    #[tokio::test]
    async fn test_a_probe_that_hangs_is_stopped() {
        let started = Instant::now();
        let result = run_probe(shell("exec sleep 30"), SHORT).await;
        assert!(result.unwrap_err().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn test_parse_duration_refuses_nonsense_without_panicking() {
        assert_eq!(parse_duration(b"12.5\n"), Ok(12.5));
        for bad in [&b"nan"[..], b"inf", b"-3", b"N/A", b"\xff\xfe", b""] {
            assert!(parse_duration(bad).is_err(), "{:?}", bad);
        }
    }

    #[tokio::test]
    async fn test_duration_of_a_missing_file_is_an_error() {
        assert!(get_video_duration("/nonexistent/file.mp4").await.is_err());
    }

    #[test]
    fn test_only_progress_lines_are_left_out_of_the_log() {
        for line in [
            "frame=10",
            "out_time_ms=5",
            "stream_0_0_q=28.0",
            "progress=end",
            "speed=1.0x",
        ] {
            assert!(is_progress_line(line), "{}", line);
        }
        for line in [
            "Input #0, mov,mp4, from 'x':",
            "[libx264 @ 0x5] using cpu capabilities",
            "Error while decoding stream #0:0",
        ] {
            assert!(!is_progress_line(line), "{}", line);
        }
    }

    // ── The FFmpeg command line ─────────────────────────────────────────────

    fn format_from(json: &str) -> VideoFormat {
        serde_json::from_str(json).unwrap()
    }

    fn built_args(format: &str, is_gpu: bool, pin_streams: bool) -> Vec<String> {
        crate::test_support::storage();
        let format = format_from(format);
        build_ffmpeg_command("in.mp4", "name", is_gpu, &format, 60.0, None, pin_streams)
            .unwrap()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn has(args: &[String], flag: &str, value: &str) -> bool {
        args.windows(2).any(|w| w[0] == flag && w[1] == value)
    }

    #[test]
    fn test_ffmpeg_never_waits_on_stdin() {
        let args = built_args(r#"{"id":1,"ext":"mp4","vcodec":"libx264"}"#, false, false);
        assert!(args.contains(&"-nostdin".to_string()));
    }

    /// Before `-i` it is taken as an input option, and FFmpeg refuses to start: every
    /// CPU video encode failed. Only some encoders accept it at all.
    #[test]
    fn test_cpu_used_is_only_given_to_encoders_that_have_it_and_after_the_input() {
        let args = built_args(r#"{"id":1,"ext":"mp4","vcodec":"libx264"}"#, false, false);
        assert!(!args.contains(&"-cpu-used".to_string()));

        let args = built_args(
            r#"{"id":1,"ext":"webm","vcodec":"libaom-av1"}"#,
            false,
            false,
        );
        let cpu_used = args.iter().position(|a| a == "-cpu-used").unwrap();
        let input = args.iter().position(|a| a == "-i").unwrap();
        assert!(input < cpu_used);
    }

    #[test]
    fn test_a_gpu_audio_format_gets_its_codec_and_no_video() {
        let args = built_args(
            r#"{"id":14,"ext":"flac","acodec":"flac","compression_level":5,"gpu":true}"#,
            true,
            false,
        );
        assert!(has(&args, "-acodec", "flac"));
        assert!(has(&args, "-compression_level", "5"));
        assert!(args.contains(&"-vn".to_string()));
    }

    #[test]
    fn test_a_cpu_audio_format_gets_its_bitrate_and_no_video() {
        let args = built_args(
            r#"{"id":8,"ext":"opus","acodec":"libopus","b_a":"128k"}"#,
            false,
            false,
        );
        assert!(has(&args, "-acodec", "libopus"));
        assert!(has(&args, "-b:a", "128k"));
        assert!(args.contains(&"-vn".to_string()));
    }

    #[test]
    fn test_an_empty_video_codec_means_there_is_no_video() {
        for is_gpu in [true, false] {
            let args = built_args(
                r#"{"id":8,"ext":"flac","vcodec":"","acodec":"flac"}"#,
                is_gpu,
                false,
            );
            assert!(!args.contains(&"-c:v".to_string()));
            assert!(has(&args, "-acodec", "flac"));
        }
    }

    #[test]
    fn test_a_cpu_format_naming_no_codec_is_refused() {
        crate::test_support::storage();
        let format = format_from(r#"{"id":1,"ext":"mp4"}"#);
        assert!(build_ffmpeg_command("in.mp4", "name", false, &format, 60.0, None, false).is_err());
    }

    /// The tap reads the first video stream, so what is published has to come from
    /// that stream too. Left to itself FFmpeg picks the biggest, which isn't scanned.
    #[test]
    fn test_an_armed_gate_makes_untapped_video_use_the_stream_it_scans() {
        let video = r#"{"id":1,"ext":"mp4","vcodec":"libx264"}"#;
        let args = built_args(video, false, true);
        assert!(has(&args, "-map", "0:v:0?"));
        assert!(has(&args, "-map", "0:a:0?"));

        let args = built_args(video, false, false);
        assert!(!args.contains(&"-map".to_string()), "gate off: unchanged");

        let audio = r#"{"id":8,"ext":"opus","acodec":"libopus"}"#;
        assert!(!built_args(audio, false, true).contains(&"-map".to_string()));
    }

    #[test]
    fn test_file_names_from_a_request_cannot_leave_the_output_directory() {
        for bad in ["../../etc/x", "mp4/../x", "a b", ""] {
            let format = format_from(&format!(r#"{{"id":1,"ext":"{}"}}"#, bad));
            assert!(validate_format(&format).is_err(), "{:?}", bad);
        }
        assert!(validate_format(&format_from(r#"{"id":1,"ext":"mp4"}"#)).is_ok());
    }

    /// Filters can read any file: `movie=` would put held content, or anything else
    /// on disk, into a published output that was never scanned.
    #[test]
    fn test_only_filters_that_cannot_read_files_are_accepted() {
        for good in [
            "scale=1920x1080",
            "scale=1280:720",
            "scale='min(1280,iw)':-2",
            "scale=640:360,fps=30",
            "format=yuv420p",
            " scale=640:360",
        ] {
            assert!(vf_allowed(good), "{}", good);
        }
        for bad in [
            "movie=/data/downloads/held[a];[in][a]overlay",
            "scale=640:360,movie=/etc/passwd",
            "movie=/etc/passwd",
            "subtitles=/etc/passwd",
            "drawtext=textfile=/etc/passwd",
            "scale='640':360,'movie=x'",
            r"scale=640\,movie=x",
            "scale='640",
            "[in]scale=640:360[out]",
            "",
        ] {
            assert!(!vf_allowed(bad), "{}", bad);
        }
    }

    /// A trimmed output is meant to be shorter, and mustn't read as a mismatch.
    #[test]
    fn test_the_expected_length_allows_for_a_trim() {
        let trimmed = format_from(r#"{"id":1,"ext":"mp4","trim_percent":20}"#);
        assert_eq!(expected_output_length(&trimmed, 120.0), 24.0);
        assert!(!duration_mismatch(
            expected_output_length(&trimmed, 120.0),
            24.0
        ));

        let full = format_from(r#"{"id":1,"ext":"mp4"}"#);
        assert_eq!(expected_output_length(&full, 120.0), 120.0);

        let hls = format_from(r#"{"id":1,"ext":"mp4","hls":true,"trim_percent":20}"#);
        assert_eq!(
            expected_output_length(&hls, 120.0),
            120.0,
            "HLS ignores the trim"
        );
    }

    /// Small differences are normal, and a false alarm on every job would get ignored.
    #[test]
    fn test_only_a_real_difference_in_length_counts_as_a_mismatch() {
        for (expected, measured, mismatch) in [
            (120.0, 120.001, false),
            (120.0, 120.9, false),
            (120.0, 121.5, true),
            (120.0, 4.0, true),
            (4.0, 120.0, true),
            (36000.0, 120.0, true),
            // A long title: 1% of it, not 1 s.
            (7200.0, 7250.0, false),
            (7200.0, 7300.0, true),
            // Nothing was claimed, so there's nothing to disagree with.
            (0.0, 120.0, false),
        ] {
            assert_eq!(
                duration_mismatch(expected, measured),
                mismatch,
                "expected {} measured {}",
                expected,
                measured
            );
        }
    }

    // ── Local files of a job ────────────────────────────────────────────────

    fn local_output(file_name: &str, ext: &str, encrypt_flag: bool) -> LocalOutput {
        let local_file = crate::shared::LocalFile::file(&plain_output_path(file_name, ext));
        LocalOutput {
            task_id: "task".to_string(),
            format_index: 0,
            file_name: file_name.to_string(),
            ext: ext.to_string(),
            dest: None,
            encrypt_flag,
            is_hls: false,
            encoded_duration: 1.0,
            preview_percent: 0,
            video_format: serde_json::json!({}),
            local_file,
        }
    }

    /// Encryption failing used to be logged and ignored: whatever was on disk got
    /// uploaded, and the CID was built with an empty key.
    #[tokio::test]
    async fn test_a_failed_encryption_stops_the_publish() {
        crate::test_support::portal();
        let name = format!("missing-{}", uuid::Uuid::new_v4());
        let result = transcode_video_publish(local_output(&name, "mp4", true)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_publishing_removes_the_local_output_whatever_happens() {
        crate::test_support::portal();
        let _uploads = crate::test_support::UPLOAD_TEST_LOCK.lock().await;
        let name = format!("published-{}", uuid::Uuid::new_v4());
        let path = plain_output_path(&name, "mp4");
        std::fs::write(&path, b"output").unwrap();
        let encrypted = format!("{}{}.mp4", *PATH_TO_TRANSCODED_FILE, name);

        let response = transcode_video_publish(local_output(&name, "mp4", true))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(response.status_code, 200);
        assert!(!Path::new(&path).exists(), "decrypted output left on disk");
        assert!(
            !Path::new(&encrypted).exists(),
            "encrypted copy left on disk"
        );
    }

    #[test]
    fn test_there_is_one_spelling_of_each_local_output_path() {
        crate::test_support::storage();
        assert_eq!(
            local_output_path("name", "mp4", false),
            plain_output_path("name", "mp4")
        );
        assert_eq!(
            local_output_path("name", "mp4", true),
            hls_output_dir("name")
        );
    }

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

    #[tokio::test]
    async fn test_source_has_video_failclosed_on_probe_error() {
        // A non-zero ffprobe exit (missing/corrupt path) — or ffprobe absent — must NOT
        // read as audio-only; the A4 gate depends on this returning true (HOLD).
        assert!(source_has_video("/nonexistent/definitely/not/a/file.xyz").await);
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
        // The standalone `-vf` must be suppressed exactly when the tap folds it
        // into the filtergraph — i.e. gated on `!do_tap`, NOT on `tap_dir`.
        // `do_tap` additionally requires a foldable vf and a video codec, so
        // keying on tap_dir dropped `-vf` from any format that was handed a tap
        // dir but rejected by either check, silently producing an unscaled
        // rendition.
        assert!(
            src.contains("if !do_tap && !audio_only {"),
            "the standalone -vf must be gated on !do_tap"
        );
    }

    /// A `[k]` pad created but never mapped makes FFmpeg fail at graph
    /// configure ("Filter ... has an unconnected output"). On the tap-owning
    /// format that means the whole rendition produces nothing while the job
    /// still publishes its other renditions — a silent loss of gate coverage,
    /// and in `enforce` an empty keyframe set holds every video job.
    /// Reported from production 2026-07-31; pinned here so it cannot recur.
    #[test]
    fn test_tap_graph_has_no_dangling_pad() {
        for vf in [None, Some("scale=1920x1080")] {
            let mut cmd = Command::new("ffmpeg");
            append_tap_pre(&mut cmd, vf, "0.50000");
            cmd.arg("-y").arg("/tmp/out_ue.mp4"); // the main output
            append_tap_post(&mut cmd, "/tmp/kf", 3600.0);
            let args: Vec<String> = cmd
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            let i = args
                .iter()
                .position(|a| a == "-filter_complex")
                .expect("tap builds a filter_complex");
            let graph = &args[i + 1];
            for label in ["m", "k"] {
                let pad = format!("[{}]", label);
                assert!(graph.contains(&pad), "graph must define {}", pad);
                assert!(
                    args.windows(2).any(|w| w[0] == "-map" && w[1] == pad),
                    "vf={:?}: the graph defines {} but nothing maps it to an output — \
                     FFmpeg fails at graph configure and the rendition produces nothing.\n\
                     argv: {}",
                    vf,
                    pad,
                    args.join(" ")
                );
            }
        }
    }

    /// The tap's two halves live in one command builder: one `append_tap_pre`, one
    /// `append_tap_post` after it. An early return slipped between them, or a second
    /// pre site, reintroduces the dangling pad above.
    #[test]
    fn test_every_tap_pre_is_matched_by_the_single_tap_post() {
        let src = include_str!("transcode_video.rs");
        let body = &src[src
            .find("fn build_ffmpeg_command(")
            .expect("command builder present")..];
        let body = &body[..body.find("\nasync fn ").unwrap_or(body.len())];
        let pre = body.matches("append_tap_pre(&mut cmd").count();
        let post = body.matches("append_tap_post(&mut cmd").count();
        assert_eq!(
            pre, 1,
            "GPU and CPU paths share one place that folds the tap"
        );
        assert_eq!(post, 1, "exactly one tap output");
        let the_pre = body.find("append_tap_pre(&mut cmd").unwrap();
        let the_post = body.find("append_tap_post(&mut cmd").unwrap();
        assert!(
            the_pre < the_post,
            "the [k] consumer must be appended after its producer"
        );
    }

    /// `av1_nvenc` accepts profile `main` only — `high` is an h264 profile.
    /// Reported from production 2026-07-31 (one entry; there were in fact three
    /// across the two shipped files).
    ///
    /// NOTE: `profile` is currently parsed into `VideoFormat` but never emitted
    /// to FFmpeg (see the dead field), so a bad value is inert today and cannot
    /// be what failed a real encode. This pins the data for whenever it is
    /// wired up.
    #[test]
    fn test_shipped_formats_have_valid_nvenc_profiles() {
        for (name, src) in [
            (
                "video_formats.json",
                include_str!("../settings/video_formats.json"),
            ),
            (
                "video_formats1.json",
                include_str!("../settings/video_formats1.json"),
            ),
        ] {
            let v: serde_json::Value = serde_json::from_str(src).expect("valid JSON");
            for f in v.as_array().expect("array of formats") {
                let vcodec = f.get("vcodec").and_then(|x| x.as_str()).unwrap_or("");
                let profile = f.get("profile").and_then(|x| x.as_str());
                if vcodec == "av1_nvenc" {
                    assert!(
                        matches!(profile, None | Some("main")),
                        "{} id {:?}: av1_nvenc accepts profile 'main' only, found {:?}",
                        name,
                        f.get("id"),
                        profile
                    );
                }
            }
        }
    }

    /// Host workaround (2026-07-31): NVENC on the production L40S fails with
    /// "Failed locking bitstream buffer" once B-frame reordering engages.
    #[test]
    fn test_nvenc_encoders_get_bf_zero() {
        for (vcodec, expect_bf) in [
            ("h264_nvenc", true),
            ("hevc_nvenc", true),
            ("av1_nvenc", true),
            ("libx264", false),
            ("libaom-av1", false),
        ] {
            let mut cmd = Command::new("ffmpeg");
            add_video_codec(&mut cmd, vcodec);
            let args: Vec<String> = cmd
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            assert!(
                args.windows(2).any(|w| w[0] == "-c:v" && w[1] == vcodec),
                "{} must still be selected",
                vcodec
            );
            assert_eq!(
                args.windows(2).any(|w| w[0] == "-bf" && w[1] == "0"),
                expect_bf,
                "{}: -bf 0 presence (NVENC only)",
                vcodec
            );
        }
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
