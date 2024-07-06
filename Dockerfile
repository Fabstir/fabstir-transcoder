# Build stage
FROM rust:1.67 as build

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
FROM debian:bullseye-slim

WORKDIR /usr/local/bin

RUN apt-get update && \
  apt-get install -y ffmpeg && \
  apt-get install -y openssl ca-certificates

# Copy the root CA certificate to the container
RUN echo "$S5_ROOT_CA" > /usr/local/share/ca-certificates/s5-root-ca.crt \
  && chmod 644 /usr/local/share/ca-certificates/s5-root-ca.crt \
  && update-ca-certificates

RUN mkdir -p ./path/to/file && chmod 777 ./path/to/file
RUN mkdir -p ./temp/to/transcode && chmod 777 ./temp/to/transcode

# Copy transode-server binary from build stage 
COPY --from=build /usr/src/transcode-example/transcode_server/target/release/transcode-server .

# Expose port 50051 
EXPOSE 50051 

# Export LD_LIBRARY_PATH 
ENV LD_LIBRARY_PATH=/usr/local/bin 

# Set transode-server binary as entrypoint 
ENTRYPOINT ["./transcode-server"]
