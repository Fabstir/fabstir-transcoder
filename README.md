# Fabstir transcoder

## Summary

This transcoder server can convert from any video format ffmpeg supports to AV1 codec. AV1 is an open-source, royalty-free video codec that has better compression than h264 with smaller file sizes and hence less bandwidth. There is also support to transcode to h264, higher resolutions and frame rates.
AV1 is already supported by a number of popular browsers, including Google Chrome, Mozilla Firefox, and Microsoft Edge. It is also supported by a growing number of video players and streaming services.

## Encoding

AV1 compression requires much more complexity than h264. Even with top of the line CPU, encoding rates are much less than real-time. Hardware encoding is realistically required and this is available on GPUs such as NVIDIA RTX 4000 series, NVIDIA A6000 or Intel Arc GPUs.

## Encryption

The transcoder offers two forms of operation; either the source video is encrypted and the transcoder will also encrypt the transcoded videos, or the source video is not encrypted thus the transcoded videos will not be encrypted.

## Technology used

The transcoder network integrates to S5 for its content delivery network (CDN) and its ability to store content to Sia cloud storage.

S5 is a content-addressed storage network similar to IPFS, but with some new concepts and ideas to make it more efficient and powerful.
https://github.com/s5-dev

Sia is a decentralized cloud storage platform that uses blockchain technology to store data https://sia.tech/. It is designed to be more secure, reliable, and affordable than traditional cloud storage providers. Sia encrypts and distributes your files across a decentralized network of hosts. This means that your data is not stored in a single location, and it is not accessible to anyone who does not have the encryption key.

# Overall workflow

![Fabstir Transcoder Workflow](https://fabstir.com/img2/Fabstir_transcoder_workflow.svg)

The user submits a POST request to the trancode RESTful API, including a payload with the `cid` (content identifier) and an array of media formats to transcode to. The transcoder supports two storage solutions for the transcoded videos: Sia via the S5 content-addressed storage layer and IPFS. The user can choose whether to encrypt the transcoded videos and whether to use GPUs or CPUs for transcoding.

Upon receiving the request, the transcoder server responds with a JSON message, including a `status_code` and a `task_id`. The `status_code` indicates whether the transcoder received the request, and the `task_id` is a unique identifier for the transcoding request. _Note that the `task_id`, is currently the `cid` but it will be changed to a unique identifier in the next version of the transcoder._

The transcoder server then transcodes the source video into each of the specified formats and uploads the transcoded videos to the specified storage solution.

The user can query the status of the transcoding job by calling the `get_transcoded` RESTful API endpoint with the `task_id` as a parameter. If the `task_id` is not valid, the user receives a 404 `status_code`. If the transcoding job has not finished then the `progress` integer value returned will be less than 100 and the `metadata` media formats array will be empty. If the transcoding job has finished, the user receives a `progress` of 100 and the `metadata` array of media format JSON objects where each media format object has an additional `src` property that gives the `cid` of the video, prefixed with either `s5://` or `ipfs://` to indicate the storage location.

# To get started

```
cd transcode_server
cargo build
cargo run transcode-server
```

# To use for video

Either use http/2:
The `.proto` file

```
message TranscodeRequest {
    string source_cid = 1;
    string media_formats = 2;
    bool is_encrypted = 3;
    bool is_gpu = 4;
}

message TranscodeResponse {
    int32 status_code = 1;
    string message = 2;
    string cid = 3;
}

service TranscodeService {
    rpc Transcode(TranscodeRequest) returns (TranscodeResponse);

    rpc GetTranscoded(GetTranscodedRequest) returns (GetTranscodedResponse);
}

message GetTranscodedRequest {
    string source_cid = 1;
}

message GetTranscodedResponse {
    int32 status_code = 1;
    string metadata = 2;
    int32 progress = 3;
}
```

Or http/1:
use port: 50051

```
      const isEncrypted = false;
      const isGPU = true;
      const cid = `${PORTAL_URL}/s5/blob/${uploadedFileCID}`;

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
        {
          id: 34,
          label: '2160p',
          type: 'video/mp4',
          ext: 'mp4',
          vcodec: 'av1_nvenc',
          preset: 'slower',
          profile: 'high',
          ch: 2,
          vf: 'scale=3840x2160',
          b_v: '18M',
          ar: '48k',
          gpu: true,
          dest: "ipfs",
        },
      ];

const url = `${TRANSCODER_CLIENT_URL}/transcode?source_cid=${cid}&media_formats=${videoFormatsJSON}&is_encrypted=${isEncrypted}&is_gpu=${isGPU}`;
try {
        const response = await fetch(url, { method: "POST" });
        const data = await response.json();
      } catch (error) {
        console.error(error);
      }
```

For example JavaScript code that uses transcoder, go [here](https://github.com/Fabstir/upload-play-example)

# To use for audio

Or http/1:
use port: 50051

```
      const isEncrypted = false;
      const isGPU = false;
      const cid = `${PORTAL_URL}/s5/blob/${uploadedFileCID}`;

      const audioFormats = [
      {
        id: 16,
        label: "1600k",
        type: "audio/flac",
        ext: "flac",
        acodec: "flac",
        ch: 2,
        ar: "48k",
      },
      ];

const url = `${TRANSCODER_CLIENT_URL}/transcode?source_cid=${cid}&media_formats=${audioFormatsJSON}&is_encrypted=${isEncrypted}&is_gpu=${isGPU}`;
try {
        const response = await fetch(url, { method: "POST" });
        const data = await response.json();
      } catch (error) {
        console.error(error);
      }
```

For example React program code that uses transcoder, go [here](https://github.com/Fabstir/upload-play-audio-example)

# Media format properties

The two previous sections show example JSON files that specify the transcoded media formats to output from a source file. These are the JSON file object properties currently supported (some have a direct one-to-one relationship with ffmpeg):
id: u32,
ext: String,
vcodec: Option&lt;String&gt;,
acodec: Option&lt;String&gt;,
preset: Option&lt;String&gt;,
profile: Option&lt;String&gt;,
ch: Option<u8>,
vf: Option<String>,
b_v: Option<String>,
ar: Option<String>,
minrate: &lt;String&gt;,
maxrate: &lt;String&gt;,
bufsize: &lt;String&gt;,
gpu: Option<bool>,
compression_level: &lt;Option<u8>&gt;,
dest: &lt;String&gt;,

Note that `dest` can be specfied for each output format type as either "s5" for uploading transcoded files to Sia via S5, "ipfs" for InterPlanetary File System or missed out from the JSON file where it will default to s5.

# Caching

The transcoder now checks to see if a source media file has already been downloaded. If so and it is still available in its cache area, it will not download again but use the local version. Similarly, if a file for a specific media format has already been transcoded and is still available in the cache area, then transcoding of the source media file for that particular format will be skipped and the local version uploaded instead.

In the `.env` file, set FILE_SIZE_THRESHOLD and TRANSCODED_FILE_SIZE_THRESHOLD to the size in bytes, above which files in the cache get deleted; starting from oldest file first. GARBAGE_COLLECTOR_INTERVAL is the polling frequency in seconds for how often these thresholds are checked.


# Deployment on k8

There are few things to take into consideration when deploying this service on k8:

1. The kuberenetes Node has to support GPU (with encoder), A16 Nvidia GPU should suffice
2. The pod in order to utilize the GPU properly has to have `nvidia-smi` 
3. installation of ffmpeg with `--enable-cuda-nvcc`
```
git clone https://git.videolan.org/git/ffmpeg/nv-codec-headers.git \
  && cd nv-codec-headers && make install && cd - \
  && git clone https://git.ffmpeg.org/ffmpeg.git \
  && cd ffmpeg/ \
  && ./configure --prefix=/usr --enable-nonfree --enable-cuda-nvcc --enable-libnpp --extra-cflags=-I/usr/local/cuda/include --extra-ldflags=-L/usr/local/cuda/lib64 --disable-static --enable-shared \
  && make -j 8 \
  && make install && ldconfig
```
4. the env variable `NVIDIA_DRIVER_CAPABILITIES=video,compute,utility`
5. for multiple pods the env variables `PATH_TO_FILE=/mnt/shared-wd/path/to/file/` and `PATH_TO_TRANSCODED_FILE=/mnt/shared-wd/temp/to/transcode/` should point to shared mounted directories, so multiple pods access same working directory.
If those are set to local directory of the pod, it is quite possible that when transcoding job is distributed, the transcoded output won't be accessible to other pods.