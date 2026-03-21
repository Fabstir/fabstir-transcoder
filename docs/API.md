# Fabstir Transcoder API Reference

The Fabstir Transcoder exposes a REST API on port **8000** and a gRPC API on port **50051**. Both provide identical transcoding functionality. This document covers both interfaces, authentication, request/response formats, and configuration.

---

## Table of Contents

- [Quick Start](#quick-start)
- [Authentication](#authentication)
- [REST API](#rest-api)
  - [GET /health](#get-health)
  - [POST /transcode](#post-transcode)
  - [GET /get_transcoded/{task_id}](#get-get_transcodedtask_id)
- [gRPC API](#grpc-api)
  - [Transcode](#rpc-transcode)
  - [GetTranscoded](#rpc-gettranscoded)
- [Transcoding Workflow](#transcoding-workflow)
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

### POST /transcode

Queue a new transcoding job. Returns immediately with a `task_id` for polling.

**Request**

```
POST /transcode?source_cid=<cid>&media_formats=<json>&is_encrypted=<bool>&is_gpu=<bool>
Authorization: Bearer <JWT_TOKEN>
```

**Query Parameters**

| Parameter | Type | Required | Description |
|---|---|---|---|
| `source_cid` | string | Yes | Content identifier of the source media file. Supports `s5://` and `ipfs://` protocol prefixes. The `s5://` prefix is stripped automatically. |
| `media_formats` | string | Yes | URL-encoded JSON array of format objects (see [Media Format Configuration](#media-format-configuration)). If empty string, defaults to formats from `MEDIA_FORMATS_FILE`. |
| `is_encrypted` | bool | Yes | Whether the source file is encrypted (XChaCha20Poly1305). **When `true`, the `PORTAL_ENCRYPT_URL` env var must be set** — the server uses it instead of `PORTAL_URL` for downloads. |
| `is_gpu` | bool | Yes | Whether to use GPU acceleration (NVIDIA NVENC). Can be overridden per-format via the `gpu` field in the format object. |

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
| `status_code` | int | HTTP status code (200) |
| `metadata` | string | JSON array (as string) of transcoded format objects, each with an added `cid` field. Value is `"Transcoding in progress"` while the job is running. |
| `progress` | int | Overall progress percentage (0-100). Averaged across all output formats. |
| `duration` | float | Source media duration in seconds, as reported by ffprobe. `0.0` if the job has not yet completed or if ffprobe could not determine duration. |

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
GET /get_transcoded/{task_id}  -> progress: 0,   duration: 0.0
GET /get_transcoded/{task_id}  -> progress: 45,  duration: 0.0
GET /get_transcoded/{task_id}  -> progress: 100, duration: 125.5, metadata: "[...]"
```

- Poll until `progress == 100`
- `duration` is `0.0` until the job completes
- `metadata` is `"Transcoding in progress"` (a plain string, not JSON) until the job completes

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
  |                                 |  Queue job via mpsc channel
  |  { task_id: "abc-123" }         |
  |<--------------------------------|
  |                                 |
  |                                 |  [Background processing]
  |                                 |  1. Download source from S5/IPFS
  |                                 |  2. Decrypt if is_encrypted=true
  |                                 |  3. Extract duration via ffprobe
  |                                 |  4. For each media format:
  |                                 |     a. FFmpeg transcode (GPU or CPU)
  |                                 |     b. Update progress (0-100%)
  |                                 |     c. Encrypt output if needed
  |                                 |     d. Upload to S5/IPFS
  |                                 |     e. Record output CID
  |                                 |  5. Store metadata + duration
  |                                 |
  |  GET /get_transcoded/abc-123    |
  |-------------------------------->|
  |  { progress: 45, duration: 0 }  |
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

### Cache and Garbage Collection

The transcoder caches downloaded source files and transcoded outputs on disk. A background garbage collector periodically cleans up:
- **Source cache**: cleaned when total size exceeds `FILE_SIZE_THRESHOLD`
- **Transcoded cache**: cleaned when total size exceeds `TRANSCODED_FILE_SIZE_THRESHOLD`
- **Interval**: configurable via `GARBAGE_COLLECTOR_INTERVAL` (default: 3600 seconds)

---

## Media Format Configuration

### Format Object Fields

| Field | Type | Required | Description |
|---|---|---|---|
| `id` | int | Yes | Unique format identifier |
| `label` | string | No | Human-readable label (e.g., "1080p", "720p", "129k") |
| `type` | string | No | MIME type (e.g., "video/mp4", "audio/opus", "audio/flac") |
| `ext` | string | Yes | Output file extension (e.g., "mp4", "opus", "flac") |
| `vcodec` | string | No | Video codec. Required for video. Options: `av1_nvenc`, `libx264`, `libx265` |
| `acodec` | string | No | Audio codec. Options: `libopus`, `flac`, `aac` |
| `preset` | string | No | Encoding preset (e.g., "medium", "slower") |
| `profile` | string | No | Encoding profile (e.g., "main", "high") |
| `ch` | int | No | Audio channels (e.g., 2 for stereo) |
| `vf` | string | No | Video filter string (e.g., "scale=1920x1080") |
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
| `NVIDIA_DRIVER_CAPABILITIES` | NVIDIA container capabilities | `compute,utility,video` |
| `PINATA_API_KEY` | Pinata IPFS API key (if using Pinata) | (none) |
| `PINATA_API_SECRET` | Pinata IPFS API secret | (none) |
| `PINATA_JWT` | Pinata IPFS JWT token | (none) |

---

## Deployment

### Docker Image

Pre-built images are published to Vultr Container Registry on every push to `main`:

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
| Missing/invalid JWT token | 401 | Rejection (no JSON body) |
| Task queue full or channel closed | 500 | Rejection with `TranscodeError` |
| Invalid query parameters | 400 | Warp parameter parsing error |

### gRPC Errors

| Scenario | gRPC Code | Description |
|---|---|---|
| Task queue send failure | `INTERNAL` (13) | Failed to send transcoding task |
| Invalid video format JSON | `INVALID_ARGUMENT` (3) | Cannot parse format configuration |
| Missing video codec | `INVALID_ARGUMENT` (3) | No video codec specified in format |
| Blake3 hash failure | `INTERNAL` (13) | Error computing file hash for upload |
| File access failure | `INTERNAL` (13) | Cannot read/write transcoded files |

### Transcoding Errors

When a format fails to transcode, it is skipped and does not appear in the `metadata` array. Other formats continue processing. The `progress` field still reaches 100 when all formats have been attempted.

### Common Gotchas

| Symptom | Cause | Fix |
|---|---|---|
| `Required environment variable for PORTAL_URL not found` but `PORTAL_URL` is set | `is_encrypted=true` in the request causes the server to read `PORTAL_ENCRYPT_URL` instead. The error message is misleading — it always says "PORTAL_URL" regardless of which variable is missing. | Set `PORTAL_ENCRYPT_URL` or use `is_encrypted=false`. |
| `Invalid source CID: <cid>` | CID is missing the protocol prefix. The server requires `s5://` or `ipfs://` before the CID. | Send `s5://uCtwQM...` not `uCtwQM...`. |
| Panic at `server.rs` `garbage_collect` / `No such file or directory` | The directories specified by `PATH_TO_FILE` and `PATH_TO_TRANSCODED_FILE` don't exist. The server does not create them on startup. | Ensure directories exist before starting (e.g., `mkdir -p` in entrypoint). |
