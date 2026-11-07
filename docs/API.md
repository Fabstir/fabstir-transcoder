# Fabstir Transcoder API Reference

The Fabstir Transcoder exposes a REST API on port **8000** and a gRPC API on port **50051**. Both provide identical transcoding functionality. This document covers both interfaces, authentication, request/response formats, and configuration.

---

## Table of Contents

- [Quick Start](#quick-start)
- [Authentication](#authentication)
- [REST API](#rest-api)
  - [GET /health](#get-health)
  - [GET /status](#get-status)
  - [POST /transcode](#post-transcode)
  - [GET /get_transcoded/{task_id}](#get-get_transcodedtask_id)
- [gRPC API](#grpc-api)
  - [Transcode](#rpc-transcode)
  - [GetTranscoded](#rpc-gettranscoded)
- [Transcoding Workflow](#transcoding-workflow)
  - [Concurrent Processing](#concurrent-processing)
- [Media Format Configuration](#media-format-configuration)
- [Environment Variables](#environment-variables)
- [Deployment](#deployment)
- [Error Reference](#error-reference)

---

## Quick Start

```bash
# 1. Generate a JWT token
cd transcode_server && cargo run --bin generate-token

# 2. Start the transcoder
cargo run --bin transcode-server

# 3. Health check
curl http://localhost:8000/health
# => {"status":"ok"}

# 4. Submit a transcoding job
curl -X POST \
  "http://localhost:8000/transcode?source_cid=s5://bafkqaa...&media_formats=%5B%7B%22id%22%3A33%7D%5D&is_encrypted=false&is_gpu=true" \
  -H "Authorization: Bearer <JWT_TOKEN>"
# => {"status_code":200,"message":"Transcoding task queued","task_id":"550e8400-..."}

# 5. Poll for completion
curl "http://localhost:8000/get_transcoded/550e8400-..." \
  -H "Authorization: Bearer <JWT_TOKEN>"
# => {"status_code":200,"metadata":"[...]","progress":100,"duration":125.5}
```

---

## Authentication

All endpoints except `/health` require JWT authentication.

### Header Format

```
Authorization: Bearer <JWT_TOKEN>
```

### Token Generation

The transcoder includes a token generator binary:

```bash
cd transcode_server && cargo run --bin generate-token
```

This reads `FABSTIR_TRANSCODER_SECRET_KEY` from the `.env` file and generates a JWT with:
- **Algorithm**: HS256
- **Claims**: `{ "sub": "user_id", "exp": 10000000000 }`

### Validation

The server performs two checks:
1. The token string must exactly match the `FABSTIR_TRANSCODER_JWT` environment variable
2. The token must be a valid JWT decodable with `FABSTIR_TRANSCODER_SECRET_KEY` using HS256

### Auth Errors

| Condition | HTTP Status |
|---|---|
| Missing `Authorization` header | 401 Unauthorized |
| Token does not match `FABSTIR_TRANSCODER_JWT` | 401 Unauthorized |
| JWT signature invalid or expired | 401 Unauthorized |
| `FABSTIR_TRANSCODER_SECRET_KEY` not set | 401 Unauthorized |

---

## REST API

Base URL: `http://<host>:8000`

### GET /health

Unauthenticated health check for container orchestration and load balancers.

**Request**

```
GET /health
```

**Response** `200 OK`

```json
{
  "status": "ok"
}
```

---

### GET /status

Authenticated endpoint reporting the server's current workload. Use this for
multi-host load balancing — route new jobs to the host with the lowest
`active_jobs` or highest available capacity (`max_concurrent - active_jobs`).

**Request**

```
GET /status
Authorization: Bearer <JWT_TOKEN>
```

**Response** `200 OK`

```json
{
  "active_jobs": 2,
  "queued_jobs": 1,
  "max_concurrent": 3
}
```

| Field | Type | Description |
|-------|------|-------------|
| `active_jobs` | int | Number of transcode jobs currently running |
| `queued_jobs` | int | Number of jobs waiting for a processing slot |
| `max_concurrent` | int | Maximum simultaneous jobs (from `MAX_CONCURRENT_TRANSCODES` env var) |

**Example**

```bash
curl http://localhost:8000/status \
  -H "Authorization: Bearer <JWT_TOKEN>"
```

---

### POST /transcode

Queue a new transcoding job. Returns immediately with a `task_id` for polling.

**Request**

```
POST /transcode?source_cid=<cid>&media_formats=<json>&is_encrypted=<bool>&is_gpu=<bool>[&preview_percent=<int>]
Authorization: Bearer <JWT_TOKEN>
```

**Query Parameters**

| Parameter | Type | Required | Description |
|---|---|---|---|
| `source_cid` | string | Yes | Content identifier of the source media file. Supports `s5://` and `ipfs://` protocol prefixes. The prefix is required, and the CID itself can only contain letters, digits, `-`, `_` and `=`, optionally followed by a file extension. |
| `media_formats` | string | Yes | URL-encoded JSON array of format objects (see [Media Format Configuration](#media-format-configuration)). If empty string, defaults to formats from `MEDIA_FORMATS_FILE`. |
| `is_encrypted` | bool | Yes | Whether the source file is encrypted (XChaCha20Poly1305). **When `true`, the `PORTAL_ENCRYPT_URL` env var must be set** — the server uses it instead of `PORTAL_URL` for downloads. |
| `is_gpu` | bool | Yes | Whether to use GPU acceleration (NVIDIA NVENC). Can be overridden per-format via the `gpu` field in the format object. |
| `preview_percent` | int | No | First N% of HLS segments are unencrypted (0-100). Only applies to HLS formats. Default: 0 (all segments encrypted). This is a **request-level** parameter, not a per-format field. |

**Response** `200 OK`

```json
{
  "status_code": 200,
  "message": "Transcoding task queued",
  "task_id": "550e8400-e29b-41d4-a716-446655440000"
}
```

| Field | Type | Description |
|---|---|---|
| `status_code` | int | HTTP status code (200 on success) |
| `message` | string | Human-readable status message |
| `task_id` | string | UUID v4 identifier for polling with `/get_transcoded` |

**Response** `503 Service Unavailable`

The queue holds 100 waiting jobs. When it is full the job is refused straight away and nothing is queued, so `task_id` is empty. Try again later.

```json
{
  "status_code": 503,
  "message": "Transcode queue is full",
  "task_id": ""
}
```

**Example**

```bash
# Transcode to 1080p AV1 and 720p H.264
curl -X POST \
  "http://localhost:8000/transcode?\
source_cid=s5://bafkqaavepzrspnpfq5mctcxmz23wuv6iayuq&\
media_formats=%5B%7B%22id%22%3A33%2C%22label%22%3A%221080p%22%2C%22ext%22%3A%22mp4%22%2C%22vcodec%22%3A%22av1_nvenc%22%2C%22vf%22%3A%22scale%3D1920x1080%22%2C%22b_v%22%3A%223.5M%22%7D%5D&\
is_encrypted=false&\
is_gpu=true" \
  -H "Authorization: Bearer <JWT_TOKEN>"
```

---

### GET /get_transcoded/{task_id}

Poll the status and result of a transcoding job.

**Request**

```
GET /get_transcoded/<task_id>
Authorization: Bearer <JWT_TOKEN>
```

**Path Parameters**

| Parameter | Type | Description |
|---|---|---|
| `task_id` | string | The UUID returned by `/transcode` |

**Response** `200 OK`

```json
{
  "status_code": 200,
  "metadata": "[{\"id\":33,\"label\":\"1080p\",\"ext\":\"mp4\",\"vcodec\":\"av1_nvenc\",\"cid\":\"s5://bafkqaa...\"}]",
  "progress": 100,
  "duration": 125.5
}
```

| Field | Type | Description |
|---|---|---|
| `status_code` | int | `200` while the job is queued, running or finished. Any other value means the job has failed permanently or isn't known here (see [Failed Jobs](#failed-jobs)). The HTTP response itself is still 200. |
| `metadata` | string | JSON array (as string) of transcoded format objects, each with an added `cid` field. Value is `"Transcoding in progress"` while the job is running. |
| `progress` | int | Overall progress percentage, averaged across all output formats. A running job stays at 99 or below, including while it is being moderated and uploaded. 100 means the job has finished and its result is in `metadata`. |
| `duration` | float | Source media duration in seconds, as reported by ffprobe. Available as soon as the source has been downloaded, and from then on in every response, including for a job that later fails. `0.0` means not known: the job is still queued or downloading, or ffprobe could not determine the duration. |

**Metadata Format (parsed)**

When `progress` reaches 100, the `metadata` field contains a JSON array string. When parsed, each element looks like:

```json
[
  {
    "id": 33,
    "label": "1080p",
    "type": "video/mp4",
    "ext": "mp4",
    "vcodec": "av1_nvenc",
    "preset": "medium",
    "profile": "main",
    "ch": 2,
    "vf": "scale=1920x1080",
    "b_v": "3.5M",
    "ar": "48k",
    "gpu": true,
    "cid": "s5://bafkqaavepzrspnpfq5mctcxmz23wuv6iayuq"
  }
]
```

The `cid` field on each format object is the content identifier of the transcoded output:
- `s5://<hash>` — stored on S5 (Sia) network (default)
- `ipfs://<hash>` — stored on IPFS (when format's `dest` field is `"ipfs"`)

**Polling Pattern**

```
POST /transcode               -> task_id
     |
     v
GET /get_transcoded/{task_id}  -> progress: 0,   duration: 0.0     (queued or downloading)
GET /get_transcoded/{task_id}  -> progress: 45,  duration: 125.5
GET /get_transcoded/{task_id}  -> progress: 100, duration: 125.5, metadata: "[...]"
```

- Poll until `status_code != 200` (failed) or `progress == 100` (finished). Check `status_code` first, because `progress` is not meaningful on a failed job.
- `duration` is `0.0` until the source has been downloaded and probed, and doesn't change after that
- `metadata` is `"Transcoding in progress"` (a plain string, not JSON) until the job completes

#### Failed Jobs

A non-200 `status_code` is final, so stop polling and don't expect the job to recover. `metadata` holds the reason as plain text, not JSON. It is for logs and people, so don't parse it.

| `status_code` | Meaning | `metadata` |
|---|---|---|
| `400` | Bad input. Retrying the same request cannot help. | `Invalid source CID`, `Invalid media formats` |
| `404` | This server has no record of the task. It may never have been accepted, it may have ended more than `JOB_RECORD_TTL_SECS` ago, or the server has restarted since. Submit the job again if it is still needed. | `Unknown task` |
| `500` | Anything else. | `Source download failed`, `Source metadata could not be read`, `Source decryption failed`, `Source could not be stored`, `All formats failed`, `Job timed out`, `Server configuration error`, `Internal error` |

```json
{
  "status_code": 400,
  "metadata": "Invalid source CID",
  "progress": 0,
  "duration": 0.0
}
```

- **Partial success is not a failure:** if at least one format publishes, the job returns `200`, and `metadata` lists only the formats that succeeded.
- **A moderation hold is not a failure:** it returns `200`, `metadata` `[]` and `progress` 100.
- **Restarts:** jobs and their status are held in memory, so a server restart loses every queued and running job, and they then read as `404`. The server doesn't restart them, so the client needs to submit them again.

---

## gRPC API

**Port**: 50051
**Package**: `transcode`
**Service**: `TranscodeService`

### Proto Definition

```protobuf
syntax = "proto3";
package transcode;

service TranscodeService {
    rpc Transcode(TranscodeRequest) returns (TranscodeResponse);
    rpc GetTranscoded(GetTranscodedRequest) returns (GetTranscodedResponse);
}

message TranscodeRequest {
    string source_cid = 1;
    string media_formats = 2;
    bool is_encrypted = 3;
    bool is_gpu = 4;
    uint32 preview_percent = 5;
}

message TranscodeResponse {
    int32 status_code = 1;
    string message = 2;
    string task_id = 3;
}

message GetTranscodedRequest {
    string task_id = 1;
}

message GetTranscodedResponse {
    int32 status_code = 1;
    string metadata = 2;
    int32 progress = 3;
    double duration = 4;
}
```

### RPC: Transcode

Identical to `POST /transcode`. Queues a transcoding job and returns a task ID.

### RPC: GetTranscoded

Identical to `GET /get_transcoded/{task_id}`. Polls job status, progress, and results.

---

## Transcoding Workflow

```
Client                          Transcoder
  |                                 |
  |  POST /transcode                |
  |  source_cid, media_formats,     |
  |  is_encrypted, is_gpu           |
  |-------------------------------->|
  |                                 |  Queue job (increment QUEUED counter)
  |  { task_id: "abc-123" }         |
  |<--------------------------------|
  |                                 |
  |                                 |  [Acquire semaphore permit]
  |                                 |  (waits if MAX_CONCURRENT reached)
  |                                 |
  |                                 |  [Background processing — concurrent]
  |                                 |  1. Download source from S5/IPFS
  |                                 |  2. Decrypt if is_encrypted=true
  |                                 |  3. Extract duration via ffprobe
  |                                 |     (reported from here on)
  |                                 |  4. For each media format:
  |                                 |     a. FFmpeg transcode (GPU or CPU)
  |                                 |     b. Update progress (up to 99%)
  |                                 |     c. Encrypt output if needed
  |                                 |     d. Upload to S5/IPFS
  |                                 |     e. Record output CID
  |                                 |  5. Store metadata
  |                                 |  6. Set progress to 100%
  |                                 |  7. Release semaphore permit
  |                                 |
  |  GET /get_transcoded/abc-123    |
  |-------------------------------->|
  |  { progress: 45,                |
  |    duration: 125.5 }            |
  |<--------------------------------|
  |                                 |
  |  GET /get_transcoded/abc-123    |
  |-------------------------------->|
  |  { progress: 100,               |
  |    duration: 125.5,             |
  |    metadata: "[{cid:...}]" }    |
  |<--------------------------------|
```

### Encryption

When `is_encrypted` is `true` (or overridden per-format via the `encrypt` field):
- Source files are decrypted using **XChaCha20Poly1305** after download
- Transcoded outputs are encrypted before upload
- Output CIDs reflect the encrypted content

### GPU Acceleration

When `is_gpu` is `true` (or overridden per-format via the `gpu` field):
- FFmpeg uses NVIDIA NVENC hardware encoders (`av1_nvenc`, `h264_nvenc`, etc.)
- Requires NVIDIA GPU with CUDA support and nvidia-container-toolkit in Docker
- **NVENC session limit**: Consumer GPUs support 3–5 concurrent NVENC sessions; professional cards (e.g., A100, L40) have no limit. Set `MAX_CONCURRENT_TRANSCODES` to stay within your GPU's session limit.

### Concurrent Processing

The transcoder processes multiple jobs concurrently, bounded by `MAX_CONCURRENT_TRANSCODES` (default: 3). When all slots are occupied, new jobs wait in the queue until a slot opens. Up to 100 can wait, and beyond that new jobs are refused with a `503`.

One job can't stop the others:

- **A failure or panic** ends that job with a `500`, and the next one starts.
- **A stuck FFmpeg** is killed once its media clock has stood still for `FFMPEG_STALL_TIMEOUT_SECS` (default: 15 minutes). The format fails, and any others carry on.
- **A job stuck anywhere else** is stopped at `JOB_DEADLINE_SECS` (default: 24 hours) and reads as `Job timed out`.
- **If the queue or cleanup task dies,** it restarts, and the jobs waiting in the queue stay there.
- **If a listening port is lost,** the server exits so that the container's restart policy can bring it back. Queued and running jobs are lost in this case, as they are in any restart (see [Failed Jobs](#failed-jobs)).


- **Monitoring**: Use `GET /status` to check `active_jobs`, `queued_jobs`, and `max_concurrent`
- **Load balancing**: For multi-host deployments, poll `/status` on each host and route new jobs to the host with the most available capacity (`max_concurrent - active_jobs`)
- **GPU limit**: Set `MAX_CONCURRENT_TRANSCODES` at or below your GPU's NVENC session limit (3–5 for consumer cards, unlimited for professional cards)

### Cache and Garbage Collection

The transcoder caches downloaded source files on disk. A job's other files are named after its task, kept only as long as it needs them, and deleted when it ends. A background garbage collector periodically cleans up, oldest files first, never touching a file a running job is using:
- **Source cache**: cleaned when total size exceeds `FILE_SIZE_THRESHOLD`
- **Transcoded files**: cleaned when total size exceeds `TRANSCODED_FILE_SIZE_THRESHOLD`
- **Interval**: configurable via `GARBAGE_COLLECTOR_INTERVAL` (default: 3600 seconds)

At startup the server creates `PATH_TO_FILE` and `PATH_TO_TRANSCODED_FILE` if needed, and exits if it can't write to them. It also removes temporary files left by a previous process. Cached sources stay.

---

## Media Format Configuration

### Format Object Fields

| Field | Type | Required | Description |
|---|---|---|---|
| `id` | int | Yes | Unique format identifier |
| `label` | string | No | Human-readable label (e.g., "1080p", "720p", "129k") |
| `type` | string | No | MIME type (e.g., "video/mp4", "audio/opus", "audio/flac") |
| `ext` | string | Yes | Output file extension (e.g., "mp4", "opus", "flac"). Letters and digits only. |
| `vcodec` | string | No | Video codec. Required for video. Options: `av1_nvenc`, `libx264` |
| `acodec` | string | No | Audio codec. Options: `libopus`, `flac`, `aac` |
| `preset` | string | No | Encoding preset (e.g., "medium", "slower") |
| `profile` | string | No | Encoding profile (e.g., "main", "high") |
| `ch` | int | No | Audio channels (e.g., 2 for stereo) |
| `vf` | string | No | Video filter string (e.g., "scale=1920x1080"). A chain of these filters only, because others can read files on the server: `scale`, `scale_cuda`, `scale_npp`, `hwupload_cuda`, `hwdownload`, `format`, `fps`, `crop`, `pad`, `setsar`, `setdar`, `transpose`, `hflip`, `vflip`, `yadif`, `bwdif`. |
| `b_v` | string | No | Video bitrate (e.g., "3.5M", "12M") |
| `b_a` | string | No | Audio bitrate (e.g., "129k", "1411k") |
| `c_a` | string | No | Audio codec (alternative to `acodec`) |
| `ar` | string | No | Audio sample rate (e.g., "22k", "44k", "48k") |
| `minrate` | string | No | Minimum video bitrate |
| `maxrate` | string | No | Maximum video bitrate |
| `bufsize` | string | No | Video buffer size |
| `gpu` | bool | No | Override the request-level `is_gpu` flag for this format |
| `encrypt` | bool | No | Override the request-level `is_encrypted` flag for this format |
| `dest` | string | No | Storage destination: `"s5"` (default) or `"ipfs"` |
| `compression_level` | int | No | Audio compression level (for FLAC) |
| `trim_percent` | int | No | Keep only the first N% of duration (1–99). Omit for full duration. Used for generating free preview clips. |
| `hls` | bool | No | When `true`, output fMP4 segments instead of a single file. Response includes `segments[]` array with per-segment CIDs. |
| `hls_time` | int | No | HLS segment duration in seconds. Default: 6. Only used when `hls: true`. |

### Preset Format Examples

**Video — 1080p AV1 (GPU)**
```json
{
  "id": 33,
  "label": "1080p",
  "type": "video/mp4",
  "ext": "mp4",
  "vcodec": "av1_nvenc",
  "preset": "medium",
  "profile": "main",
  "ch": 2,
  "vf": "scale=1920x1080",
  "b_v": "3.5M",
  "ar": "48k",
  "gpu": true
}
```

**Video — 2160p AV1 (GPU)**
```json
{
  "id": 34,
  "label": "2160p",
  "type": "video/mp4",
  "ext": "mp4",
  "vcodec": "av1_nvenc",
  "preset": "slower",
  "profile": "high",
  "ch": 2,
  "vf": "scale=3840x2160",
  "b_v": "12M",
  "ar": "48k",
  "gpu": true
}
```

**Video — 720p H.264 (CPU)**
```json
{
  "id": 29,
  "label": "720p",
  "type": "video/mp4",
  "ext": "mp4",
  "vcodec": "libx264",
  "ch": 2,
  "vf": "scale=1280x720",
  "b_v": "1M",
  "ar": "44k",
  "gpu": false
}
```

**Audio — Opus 129kbps**
```json
{
  "id": 13,
  "label": "129k",
  "type": "audio/opus",
  "ext": "opus",
  "ch": 2,
  "acodec": "libopus",
  "b_a": "129k",
  "ar": "44k"
}
```

**Audio — FLAC Lossless**
```json
{
  "id": 14,
  "label": "1411k",
  "type": "audio/flac",
  "ext": "flac",
  "ch": 2,
  "acodec": "flac",
  "b_a": "1411k",
  "ar": "44k"
}
```

**Preview / Paywall Pattern**

To generate both a full encrypted video (paid) and a trimmed unencrypted preview (free) in a single request, use two format entries with `encrypt` and `trim_percent`:

```json
[
  {
    "id": 1, "ext": "mp4", "vcodec": "av1_nvenc",
    "vf": "scale=1920x1080", "b_v": "5M",
    "dest": "s5", "encrypt": true, "gpu": true
  },
  {
    "id": 2, "ext": "mp4", "vcodec": "av1_nvenc",
    "vf": "scale=1280x720", "b_v": "2M",
    "trim_percent": 20, "encrypt": false,
    "dest": "s5", "gpu": true
  }
]
```

Format 1 produces the full-length encrypted output (paid content CID). Format 2 produces a 20% duration preview at lower resolution, uploaded unencrypted (free preview CID). Both CIDs are returned in the `metadata` array of the `GetTranscoded` response.

**HLS Adaptive Streaming**

For adaptive bitrate streaming, use `hls: true` on each format. The `preview_percent` query parameter controls which segments are unencrypted (preview) vs encrypted (paid). The SDK builds `.m3u8` playlists from the returned segment CIDs.

```json
[
  {
    "id": 1, "ext": "mp4", "vcodec": "av1_nvenc",
    "vf": "scale=1920x1080", "b_v": "5M",
    "hls": true, "hls_time": 6, "gpu": true
  },
  {
    "id": 2, "ext": "mp4", "vcodec": "av1_nvenc",
    "vf": "scale=1280x720", "b_v": "2.5M",
    "hls": true, "hls_time": 6, "gpu": true
  },
  {
    "id": 3, "ext": "mp4", "vcodec": "av1_nvenc",
    "vf": "scale=854x480", "b_v": "1.15M",
    "hls": true, "hls_time": 6, "gpu": true
  },
  {
    "id": 4, "ext": "mp4", "vcodec": "av1_nvenc",
    "vf": "scale=640x360", "b_v": "0.6M",
    "hls": true, "hls_time": 6, "gpu": true
  }
]
```

With `preview_percent=15`, the response for each HLS format includes:

```json
{
  "id": 1, "ext": "mp4", "vcodec": "av1_nvenc",
  "vf": "scale=1920x1080", "b_v": "5M",
  "hls": true,
  "initSegmentCid": "s5://zInitSeg...",
  "segments": [
    { "index": 0, "cid": "s5://zSeg0...", "duration": 6.006, "encrypted": false },
    { "index": 1, "cid": "s5://zSeg1...", "duration": 6.006, "encrypted": false },
    { "index": 15, "cid": "s5://uSeg15...", "duration": 6.006, "encrypted": true }
  ],
  "previewSegments": 15,
  "totalSegments": 100,
  "totalDuration": 598.764
}
```

- Segments with `index < previewSegments` have `encrypted: false` and `s5://z` CID prefix (unencrypted)
- Segments with `index >= previewSegments` have `encrypted: true` and `s5://u` CID prefix (encrypted, key embedded in CID)
- `initSegmentCid` is always unencrypted
- Non-HLS formats in the same request still return a single `cid` field
- Progress reports 0-70% during FFmpeg encoding, 70-100% during segment upload

### Available Resolutions

| Label | Scale Filter | Typical Video Bitrate (AV1) | Typical Video Bitrate (H.264) |
|---|---|---|---|
| 144p | `scale=256x144` | 0.1-0.2M | 0.3-0.5M |
| 240p | `scale=426x240` | 0.4M | 0.25M |
| 360p | `scale=640x360` | 0.6M | 0.5-0.7M |
| 480p | `scale=854x480` | 1.15M | 0.65M |
| 720p | `scale=1280x720` | 2.75M | 1M |
| 1080p | `scale=1920x1080` | 3.5-4.5M | 1.25M |
| 1440p | `scale=2560x1440` | 8M | - |
| 2160p | `scale=3840x2160` | 12-18M | - |

### Default Formats File

If `media_formats` is an empty string in the request, the server loads formats from the file specified by the `MEDIA_FORMATS_FILE` environment variable.

---

## Environment Variables

Configure the transcoder via a `.env` file in the `transcode_server/` directory.

### Required

| Variable | Description | Example |
|---|---|---|
| `PORTAL_URL` | S5 gateway URL for non-encrypted uploads/downloads | `https://s5.example.com` |
| `TOKEN` | S5 authentication token | `s5-auth-token-value` |
| `PATH_TO_FILE` | Directory for cached source downloads | `/data/downloads/` |
| `PATH_TO_TRANSCODED_FILE` | Directory for cached transcoded files | `/data/transcoded/` |
| `MEDIA_FORMATS_FILE` | Path to default video formats JSON | `./settings/video_formats.json` |
| `FABSTIR_TRANSCODER_JWT` | Expected JWT token value for API auth | `eyJ0eXAiOi...` |
| `FABSTIR_TRANSCODER_SECRET_KEY` | Secret key for JWT signing/verification (HS256) | `my-secret-key` |

### Optional

| Variable | Description | Default |
|---|---|---|
| `PORTAL_ENCRYPT_URL` | S5 gateway URL for encrypted file downloads/uploads. **Required if any request uses `is_encrypted=true`** — the server reads this instead of `PORTAL_URL` for encrypted files. | (none) |
| `IPFS_GATEWAY` | IPFS gateway URL for `ipfs://` CIDs | (none) |
| `FILE_SIZE_THRESHOLD` | Source cache cleanup threshold (bytes) | `1000000000` (1 GB) |
| `TRANSCODED_FILE_SIZE_THRESHOLD` | Transcoded cache cleanup threshold (bytes) | `1000000000` (1 GB) |
| `GARBAGE_COLLECTOR_INTERVAL` | Cache cleanup interval (seconds) | `3600` (1 hour) |
| `MAX_CONCURRENT_TRANSCODES` | Maximum number of transcode jobs processed simultaneously | `3` |
| `FFMPEG_STALL_TIMEOUT_SECS` | Kill FFmpeg once its media clock has not moved for this long. `0` turns this off. | `900` (15 minutes) |
| `JOB_DEADLINE_SECS` | Stop a job that has run for this long since it got a slot. Set it above your longest real job. `0` turns this off. | `86400` (24 hours) |
| `JOB_RECORD_TTL_SECS` | How long the result of a finished job can still be fetched, before it reads as `404`. | `86400` (24 hours) |
| `NVIDIA_DRIVER_CAPABILITIES` | NVIDIA container capabilities | `compute,utility,video` |
| `PINATA_API_KEY` | Pinata IPFS API key (if using Pinata) | (none) |
| `PINATA_API_SECRET` | Pinata IPFS API secret | (none) |
| `PINATA_JWT` | Pinata IPFS JWT token | (none) |

### Content Moderation

Moderation has two independent switches, and every combination is supported:

- **`MODERATION_GATE`** is the publish gate. It sends sampled keyframes to the node and **can hold a job**.
- **`MODERATION_ENABLED`** turns on a shadow scan by the moderation sidecar. It **never blocks or delays a job**, and it needs that sidecar deployed.

While the gate is `dark` or `enforce`, the shadow scan still runs, but its verdict is not sent to the node, so the gate's result is the only one the node records.

#### Publish gate

| `MODERATION_GATE` | Behaviour |
|---|---|
| `off` (default) | No keyframe tap, no POST, no hold. |
| `dark` | Taps keyframes, POSTs them and records the verdict, logs `MODERATION WOULD-HOLD` if it would have held, and **publishes anyway**. |
| `enforce` | Publishes only if every batch returns `cleared`. Anything else holds the job, including an unreachable node, a timeout or a non-2xx response. |

Any other value stops the server at startup, with an error naming the bad value and the valid set.

- **Held jobs:** nothing is uploaded, and `GetTranscoded` returns `status_code` 200, `metadata` `[]` and `progress` 100.
- **Batching:** keyframes are sent in sequential batches of at most 200 keyframes or 12 MiB of raw PNG, whichever comes first, to stay under the node's 20 MiB request limit. Every batch carries the same `taskId` and `sourceSha256`, and the first batch that isn't `cleared` stops the run.
- **Rollout:** the gate uses the same `off` → `dark` → `enforce` steps as the node and SDK gates. Do not set `enforce` until the node can return `cleared` for this content, or every job will hold.

| Variable | Description | Default |
|---|---|---|
| `MODERATION_GATE` | Publish gate mode: `off`, `dark` or `enforce`. | `off` |
| `MODERATION_NODE_URL` | Node base URL for `POST /v1/moderate/frames`. Unset while the gate is armed ⇒ every job holds. | (none) |
| `MODERATION_INGEST_TOKEN` | Shared secret sent as the `ingestToken` body field. Unset or wrong ⇒ the node returns 401 and every job holds. | (none) |
| `MODERATION_FRAMES_TIMEOUT_SECS` | Deadline for each frames POST, per batch (seconds). | `30` |
| `MODERATION_SAMPLE_INTERVAL_SECS` | Shortest interval between sampled keyframes (seconds). | `2.0` |
| `MODERATION_MAX_INTERVAL_SECS` | Longest interval between sampled keyframes (seconds). Raised to the shortest interval if set below it. | `12.0` |
| `MODERATION_KEYFRAME_MAX` | Keyframe budget. Widens the sampling interval on long content and never truncates a source whose duration was probed. Above about 3 h 20 m the longest interval wins, and the count exceeds the budget. | `1000` |

#### Shadow scan

| Variable | Description | Default |
|---|---|---|
| `MODERATION_ENABLED` | Starts a detached sidecar scan for each job. Fails open: it never affects the job. | `false` |
| `MODERATION_SOCKET_PATH` | Path to the sidecar's Unix socket. Unset while enabled ⇒ one config-fault log per job, and the job continues. | (none) |
| `MODERATION_SIDECAR_SOURCE_ROOT` | The sidecar's mount point for the source directory, used only to translate paths. | `/sources` |
| `MODERATION_TIMEOUT_SECS` | Deadline for each sidecar call (seconds), starting once the call leaves the queue. | `10800` |
| `MODERATION_MAX_PENDING` | Maximum queued and in-flight scans. Beyond this, scans are skipped with an alert log, and the job continues. | `8` |

---

## Deployment

### Docker Image

Images are built and pushed to Vultr Container Registry by hand. No CI job publishes them, so pushing to `main` does not build or deploy anything:

```
ewr.vultrcr.com/fabstir/transcoder:{git-sha}
```

### Docker Compose (Production Reference)

See [`docker-compose.prod.yml`](../docker-compose.prod.yml) for a production-ready reference configuration with:
- All environment variables documented
- Volume mounts for cache directories
- Health check configuration
- Optional NVIDIA GPU support

### Ports

| Port | Protocol | Purpose |
|---|---|---|
| 8000 | HTTP/REST | REST API (recommended for most integrations) |
| 50051 | gRPC | gRPC API (for tonic/protobuf clients) |

### Health Check

```bash
curl -f http://localhost:8000/health
# Returns: {"status":"ok"}
```

Docker health check (built into the production Dockerfile):
```dockerfile
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD curl -f http://localhost:8000/health || exit 1
```

### Runtime Requirements

- **FFmpeg**: Required (bundled in Docker image)
- **NVIDIA GPU** (optional): For hardware-accelerated encoding. Requires `nvidia-container-toolkit`.
- **Network access**: To S5/IPFS gateways for storage operations

---

## Error Reference

### REST Errors

| Scenario | HTTP Status | Response |
|---|---|---|
| Missing/invalid JWT token | 401 | `{"status_code": 401, "message": "Invalid or missing token"}` |
| Task queue full | 503 | `{"status_code": 503, "message": "Transcode queue is full", "task_id": ""}` |
| Task queue closed | 500 | Rejection with `TranscodeError` |
| Invalid query parameters | 400 | Warp parameter parsing error |

### gRPC Errors

| Scenario | gRPC Code | Description |
|---|---|---|
| Task queue full | `RESOURCE_EXHAUSTED` (8) | Transcode queue is full |
| Task queue closed | `INTERNAL` (13) | Transcode queue is closed |
| Invalid video format JSON | `INVALID_ARGUMENT` (3) | Cannot parse format configuration |
| Missing video codec | `INVALID_ARGUMENT` (3) | No video codec specified in format |
| Blake3 hash failure | `INTERNAL` (13) | Error computing file hash for upload |
| File access failure | `INTERNAL` (13) | Cannot read/write transcoded files |

### Transcoding Errors

When a format fails to transcode, it is skipped and does not appear in the `metadata` array. Other formats continue processing, and the job finishes with `progress` 100 as long as at least one format was published. If none was, the job fails (see [Failed Jobs](#failed-jobs)).

### Common Gotchas

| Symptom | Cause | Fix |
|---|---|---|
| `Required environment variable for PORTAL_URL not found` but `PORTAL_URL` is set | `is_encrypted=true` in the request causes the server to read `PORTAL_ENCRYPT_URL` instead. The error message is misleading — it always says "PORTAL_URL" regardless of which variable is missing. | Set `PORTAL_ENCRYPT_URL` or use `is_encrypted=false`. |
| `Invalid source CID` | CID is missing the protocol prefix, or contains a path or other characters a CID can't have. The server requires `s5://` or `ipfs://` before the CID. | Send `s5://uCtwQM...` not `uCtwQM...`. |
| Server exits at startup with `FATAL: PATH_TO_FILE` or `FATAL: PATH_TO_TRANSCODED_FILE` | The variable isn't set, or the server can't create or write to that directory. | Set it, and check the mount and its permissions. |
