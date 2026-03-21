# FFmpeg build stage — builds FFmpeg from source with NVENC support
FROM nvidia/cuda:12.8.0-devel-ubuntu22.04 AS ffmpeg-build

RUN apt-get update && \
  apt-get install -y build-essential pkg-config nasm yasm git wget xz-utils \
  libx264-dev libopus-dev && \
  apt-get clean && \
  rm -rf /var/lib/apt/lists/*

# Install nv-codec-headers (NVENC/NVDEC API stubs; AV1 NVENC requires >= 12.0)
RUN git clone --branch n12.2.72.0 --depth 1 https://git.videolan.org/git/ffmpeg/nv-codec-headers.git && \
  cd nv-codec-headers && \
  make install && \
  cd .. && rm -rf nv-codec-headers

# Download, configure, and build FFmpeg 7.0.2
RUN wget -q https://ffmpeg.org/releases/ffmpeg-7.0.2.tar.xz && \
  tar xf ffmpeg-7.0.2.tar.xz && \
  cd ffmpeg-7.0.2 && \
  ./configure \
    --prefix=/usr/local \
    --enable-gpl \
    --enable-nonfree \
    --enable-libx264 \
    --enable-libopus \
    --enable-cuda-nvcc \
    --enable-libnpp \
    --extra-cflags="-I/usr/local/cuda/include" \
    --extra-ldflags="-L/usr/local/cuda/lib64" \
    --enable-shared \
    --disable-static \
    --disable-doc && \
  make -j$(nproc) && \
  make install && \
  cd .. && rm -rf ffmpeg-7.0.2 ffmpeg-7.0.2.tar.xz

# Rust build stage
FROM rust:1.72 AS build

WORKDIR /usr/src/transcode-example

# Copy the Cargo.toml and Cargo.lock files for both projects into the Docker image
COPY tus_client/Cargo.toml tus_client/Cargo.lock ./tus_client/
COPY transcode_server/Cargo.toml transcode_server/Cargo.lock ./transcode_server/

# Copy the source code and the build.rs file for both projects into the Docker image
COPY tus_client/src ./tus_client/src
COPY transcode_server/src ./transcode_server/src
COPY transcode_server/build.rs ./transcode_server/

# Set the working directory to /usr/src/transcode/transcode_server
WORKDIR /usr/src/transcode-example/transcode_server

# Install required dependencies
RUN apt-get update && \
  apt-get install -y build-essential protobuf-compiler-grpc wget protobuf-compiler && \
  apt-get clean && \
  rm -rf /var/lib/apt/lists/*

# Sets the PROTOC environment variable to the path of the protoc binary in the Docker container
ENV PROTOC=/usr/bin/protoc

# Copy the proto directory and generate Rust code for the transcode_server project using build.rs
COPY transcode_server/proto ./proto

# Build the transcode_server project, which will also build the tus_client dependency
RUN cargo build --release --bin transcode-server

# Runtime stage
FROM nvidia/cuda:12.8.0-runtime-ubuntu22.04

WORKDIR /usr/local/bin

RUN apt-get update && \
  apt-get install -y openssl ca-certificates curl libnuma1 \
  libx264-163 libopus0 libnpp-12-8 && \
  apt-get clean && \
  rm -rf /var/lib/apt/lists/*

# Copy FFmpeg binaries and shared libraries from build stage
COPY --from=ffmpeg-build /usr/local/bin/ffmpeg /usr/local/bin/ffmpeg
COPY --from=ffmpeg-build /usr/local/bin/ffprobe /usr/local/bin/ffprobe
COPY --from=ffmpeg-build /usr/local/lib/lib*.so* /usr/local/lib/

RUN ldconfig

# Set library paths (replaces install-script.sh which wrote to ~/.profile)
ENV LD_LIBRARY_PATH="/usr/local/lib:/usr/local/cuda/lib64"

# Copy the root CA certificate to the container
RUN echo "$S5_ROOT_CA" > /usr/local/share/ca-certificates/s5-root-ca.crt \
  && chmod 644 /usr/local/share/ca-certificates/s5-root-ca.crt \
  && update-ca-certificates

RUN mkdir -p ./path/to/file && chmod 777 ./path/to/file
RUN mkdir -p ./temp/to/transcode && chmod 777 ./temp/to/transcode

# Copy transcode-server binary from build stage
COPY --from=build /usr/src/transcode-example/transcode_server/target/release/transcode-server .

# Expose ports
EXPOSE 50051
EXPOSE 8000

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD curl -f http://localhost:8000/health || exit 1

# Set transcode-server binary as entrypoint
ENTRYPOINT ["./transcode-server"]
