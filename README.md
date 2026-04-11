# Fabstir Transcoder

## Summary

A Rust video/audio transcoding server that converts media to AV1/H.264 codecs using FFmpeg, with support for GPU acceleration (NVIDIA NVENC), file encryption (XChaCha20-Poly1305), and decentralised storage via S5 (Sia) and IPFS.

The server supports two output modes:
- **Single-file** — one output file per format (existing behaviour)
- **HLS fMP4 segments** — 6-second fMP4 segments per format with per-segment encryption based on a preview boundary, enabling adaptive bitrate streaming

AV1 is an open-source, royalty-free video codec with better compression than H.264 — smaller file sizes and less bandwidth. Hardware encoding via NVIDIA NVENC (RTX 4000+ for AV1, RTX 2000+ for H.264) makes real-time and faster-than-real-time transcoding feasible.

## Encryption

The transcoder supports XChaCha20-Poly1305 encryption. In single-file mode, the `encrypt` per-format flag controls whether each output is encrypted. In HLS mode, the request-level `preview_percent` parameter determines which segments are unencrypted (free preview) and which are encrypted (paid content). Encrypted CIDs embed the decryption key — the CID itself is the access credential.

## Technology

The transcoder integrates with S5 for its content delivery network and Sia cloud storage.

S5 is a content-addressed storage network similar to IPFS, with concepts to make it more efficient and powerful. https://github.com/s5-dev

Sia is a decentralized cloud storage platform that uses blockchain technology https://sia.tech/.

## Overall Workflow

![Fabstir Transcoder Workflow](https://fabstir.com/img2/Fabstir_transcoder_workflow.svg)

1. Submit a POST request to `/transcode` with a `source_cid` and an array of media formats
2. The server responds with a `task_id`
3. The transcoder downloads the source, transcodes per format, encrypts if needed, and uploads outputs
4. Poll `GET /get_transcoded/{task_id}` until `progress == 100`
5. Parse the `metadata` JSON array — each format has a `cid` (single-file) or `segments[]` array (HLS)

For HLS formats, FFmpeg outputs fMP4 segments. Each segment is uploaded individually to S5 with per-segment encryption decisions based on `preview_percent`. The response includes segment CIDs, durations, and encryption status. The SDK generates `.m3u8` playlists from the returned data.

## Getting Started

```bash
cd transcode_server
cargo build
cargo run --bin transcode-server
```

Requires: FFmpeg, protobuf compiler (`protoc`). See [docs/API.md](docs/API.md) for full API reference and environment variable configuration.

## API

The server exposes both REST (port 8000) and gRPC (port 50051) interfaces.

### Proto Definition

```protobuf
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

service TranscodeService {
    rpc Transcode(TranscodeRequest) returns (TranscodeResponse);
    rpc GetTranscoded(GetTranscodedRequest) returns (GetTranscodedResponse);
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

### REST Endpoints

| Method | Endpoint | Auth | Purpose |
|--------|----------|------|---------|
| `GET` | `/health` | No | Health check |
| `GET` | `/status` | JWT | Active/queued/max job counts |
| `POST` | `/transcode` | JWT | Submit transcoding job |
| `GET` | `/get_transcoded/{task_id}` | JWT | Poll job progress and results |

### Video Example

```javascript
const videoFormats = [
  {
    id: 32,
    label: "1080p",
    type: "video/mp4",
    ext: "mp4",
    vcodec: "av1_nvenc",
    preset: "medium",
    profile: "main",
    ch: 2,
    vf: "scale=1920x1080",
    b_v: "4.5M",
    ar: "44k",
    gpu: true,
    dest: "s5",
  },
];

const url = `${TRANSCODER_URL}/transcode?source_cid=${cid}&media_formats=${JSON.stringify(videoFormats)}&is_encrypted=false&is_gpu=true`;
const response = await fetch(url, { method: "POST", headers: { Authorization: `Bearer ${token}` } });
const data = await response.json();
// data.task_id → poll with /get_transcoded/{task_id}
```

### HLS Adaptive Streaming Example

```javascript
const hlsFormats = [
  { id: 1, ext: "mp4", vcodec: "av1_nvenc", vf: "scale=1920x1080", b_v: "5M", hls: true, hls_time: 6, gpu: true },
  { id: 2, ext: "mp4", vcodec: "av1_nvenc", vf: "scale=1280x720", b_v: "2.5M", hls: true, hls_time: 6, gpu: true },
  { id: 3, ext: "mp4", vcodec: "av1_nvenc", vf: "scale=854x480", b_v: "1.15M", hls: true, hls_time: 6, gpu: true },
];

// preview_percent=15 → first 15% of segments unencrypted, rest encrypted
const url = `${TRANSCODER_URL}/transcode?source_cid=${cid}&media_formats=${JSON.stringify(hlsFormats)}&is_encrypted=true&is_gpu=true&preview_percent=15`;
```

The response for each HLS format includes:
```json
{
  "id": 1, "ext": "mp4", "vcodec": "av1_nvenc", "hls": true,
  "initSegmentCid": "s5://uEiB...",
  "segments": [
    { "index": 0, "cid": "s5://uEiC...", "duration": 6.006, "encrypted": false },
    { "index": 15, "cid": "s5://uEiD...", "duration": 6.006, "encrypted": true }
  ],
  "previewSegments": 15,
  "totalSegments": 100,
  "totalDuration": 598.764
}
```

### Audio Example

```javascript
const audioFormats = [
  { id: 16, label: "1600k", type: "audio/flac", ext: "flac", acodec: "flac", ch: 2, ar: "48k" },
];

const url = `${TRANSCODER_URL}/transcode?source_cid=${cid}&media_formats=${JSON.stringify(audioFormats)}&is_encrypted=false&is_gpu=false`;
```

For example JavaScript code that uses the transcoder, go [here](https://github.com/Fabstir/upload-play-example).

## Media Format Properties

| Field | Type | Description |
|-------|------|-------------|
| `id` | u32 | Unique format identifier (required) |
| `ext` | String | Output file extension, e.g. "mp4", "opus", "flac" (required) |
| `vcodec` | Option\<String\> | Video codec: `av1_nvenc`, `libx264`, `libx265` |
| `acodec` | Option\<String\> | Audio codec: `libopus`, `flac`, `aac` |
| `preset` | Option\<String\> | Encoding preset, e.g. "medium", "slower" |
| `profile` | Option\<String\> | Encoding profile, e.g. "main", "high" |
| `ch` | Option\<u8\> | Audio channels (e.g. 2 for stereo) |
| `vf` | Option\<String\> | Video filter, e.g. "scale=1920x1080" |
| `b_v` | Option\<String\> | Video bitrate, e.g. "3.5M" |
| `b_a` | Option\<String\> | Audio bitrate, e.g. "128k" |
| `c_a` | Option\<String\> | Audio codec (alternative to `acodec`) |
| `ar` | Option\<String\> | Audio sample rate, e.g. "48k" |
| `minrate` | Option\<String\> | Minimum video bitrate |
| `maxrate` | Option\<String\> | Maximum video bitrate |
| `bufsize` | Option\<String\> | Video buffer size |
| `gpu` | Option\<bool\> | Per-format GPU override |
| `compression_level` | Option\<u8\> | Audio compression level (for FLAC) |
| `dest` | Option\<String\> | Storage destination: `"s5"` (default) or `"ipfs"` |
| `encrypt` | Option\<bool\> | Per-format encryption override (single-file mode) |
| `trim_percent` | Option\<u8\> | Keep only first N% of duration (1-99, single-file mode) |
| `hls` | Option\<bool\> | Output fMP4 segments instead of single file |
| `hls_time` | Option\<u32\> | Segment duration in seconds (default 6, HLS mode only) |

## Caching

The transcoder checks if a source media file has already been downloaded. If so and it is still available in cache, it will use the local version. Similarly, if a file for a specific format has already been transcoded and is still available in cache, transcoding for that format will be skipped.

In the `.env` file, set `FILE_SIZE_THRESHOLD` and `TRANSCODED_FILE_SIZE_THRESHOLD` to the size in bytes above which cached files get deleted (oldest first). `GARBAGE_COLLECTOR_INTERVAL` is the polling frequency in seconds. The garbage collector handles both single files and HLS segment directories.

## Documentation

- [API Reference](docs/API.md) — full REST/gRPC API documentation, examples, environment variables
- [Project Overview](docs/fabstir-transcoder-overview.md) — architecture, capabilities, Fabstir v2 integration
- [HLS Transcoder Spec](docs/node-reference/HLS-TRANSCODER-SPEC.md) — node developer specification for HLS support
