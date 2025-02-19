# Build stage
FROM rust:1.72 as build

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
ENV PROTOC /usr/bin/protoc

# Copy the proto directory and generate Rust code for the transcode_server project using build.rs
COPY transcode_server/proto ./proto

# Build the transcode_server project, which will also build the tus_client dependency
RUN cargo build --release --bin transcode-server

# Runtime stage
FROM nvidia/cuda:12.8.0-devel-ubuntu22.04

WORKDIR /usr/local/bin

RUN apt-get update && \
  apt-get install -y build-essential nasm yasm cmake pkg-config libssl-dev \
  git openssl ca-certificates curl libaom-dev libsvtav1-dev python3-launchpadlib \
  libtool libc6 libc6-dev unzip wget libnuma1 libnuma-dev 
  # ffmpeg

COPY ./install-script.sh ./install-script.sh

RUN chmod +x ./install-script.sh

RUN bash ./install-script.sh


# # installing ffmpeg with cuda enabled
RUN git clone https://git.videolan.org/git/ffmpeg/nv-codec-headers.git \
  && cd nv-codec-headers && make install && cd - \
  && git clone https://git.ffmpeg.org/ffmpeg.git \
  && cd ffmpeg/ \
  && ./configure --prefix=/usr --enable-nonfree --enable-cuda-nvcc --enable-libnpp --extra-cflags=-I/usr/local/cuda/include --extra-ldflags=-L/usr/local/cuda/lib64 --disable-static --enable-shared \
  && make -j 8 \
  && make install && ldconfig

# Copy the root CA certificate to the container
RUN echo "$S5_ROOT_CA" > /usr/local/share/ca-certificates/s5-root-ca.crt \
  && chmod 644 /usr/local/share/ca-certificates/s5-root-ca.crt \
  && update-ca-certificates

RUN mkdir -p ./path/to/file && chmod 777 ./path/to/file
RUN mkdir -p ./temp/to/transcode && chmod 777 ./temp/to/transcode

# Copy transode-server binary from build stage 
COPY --from=build /usr/src/transcode-example/transcode_server/target/release/transcode-server .

# Expose port 
EXPOSE 50051
EXPOSE 8000

# # Export LD_LIBRARY_PATH 
# ENV LD_LIBRARY_PATH=/usr/local/bin 

# Set transode-server binary as entrypoint
ENTRYPOINT ["./transcode-server"]
