#![doc(html_root_url = "https://docs.rs/tus_client/0.1.1")]
use crate::http::{default_headers, Headers, HttpHandler, HttpMethod, HttpRequest};
use std::collections::HashMap;
use std::error::Error as StdError;
use std::fmt::{Display, Formatter};
use std::fs::File;
use std::io;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::num::ParseIntError;
use std::ops::Deref;
use std::path::Path;
use std::str::FromStr;

mod headers;
/// Contains the `HttpHandler` trait and related structs. This module is only relevant when implement `HttpHandler` manually.
pub mod http;

#[cfg(feature = "reqwest")]
mod reqwest;

const DEFAULT_CHUNK_SIZE: usize = 5 * 1024 * 1024;

/// Used to interact with a [tus](https://tus.io) endpoint.
pub struct Client<'a> {
    use_method_override: bool,
    http_handler: Box<dyn HttpHandler + 'a>,
    auth_token: Option<String>,
}

impl<'a> Client<'a> {
    /// Instantiates a new instance of `Client`. `http_handler` needs to implement the `HttpHandler` trait.
    /// A default implementation of this trait for the `reqwest` library is available by enabling the `reqwest` feature.
    pub fn new(http_handler: impl HttpHandler + 'a) -> Self {
        Client {
            use_method_override: false,
            http_handler: Box::new(http_handler),
            auth_token: None,
        }
    }

    /// Some environments might not support using the HTTP methods `PATCH` and `DELETE`. Use this method to create a `Client` which uses the `X-HTTP-METHOD-OVERRIDE` header to specify these methods instead.
    pub fn with_method_override(http_handler: impl HttpHandler + 'a) -> Self {
        Client {
            use_method_override: true,
            http_handler: Box::new(http_handler),
            auth_token: None,
        }
    }

    pub fn with_auth_token(mut self, auth_token: impl Into<String>) -> Self {
        self.auth_token = Some(auth_token.into());
        self
    }

    /// Retrieves information about an upload from the Tus server.
    ///
    /// # Arguments
    ///
    /// * `url` - The URL of the upload on the Tus server.
    ///
    /// # Returns
    ///
    /// A `Result` which is `Ok` if the upload information is successfully retrieved, otherwise `Err`.
    pub fn get_info(&self, url: &str) -> Result<UploadInfo, Error> {
        let req = self.create_request(HttpMethod::Head, url, None, Some(default_headers()));

        let response = self.http_handler.deref().handle_request(req)?;

        // The status first: only this can tell an upload that's gone from a server
        // that's having a bad moment, and the answer to each is very different.
        if response.status_code == 404 || response.status_code == 410 {
            return Err(Error::NotFoundError);
        }
        if !(200..300).contains(&response.status_code) {
            return Err(Error::UnexpectedStatusCode(response.status_code));
        }

        let bytes_uploaded = response
            .headers
            .get_by_key(headers::UPLOAD_OFFSET)
            .ok_or_else(|| Error::MissingHeader(headers::UPLOAD_OFFSET.to_owned()))?
            .parse::<usize>()?;

        let total_size = response
            .headers
            .get_by_key(headers::UPLOAD_LENGTH)
            .and_then(|l| l.parse::<usize>().ok());

        let metadata = response
            .headers
            .get_by_key(headers::UPLOAD_METADATA)
            .and_then(|data| base64::decode(data).ok())
            .and_then(|decoded| String::from_utf8(decoded).ok())
            .map(|decoded_str| {
                decoded_str
                    .split(';')
                    .filter_map(|key_val| {
                        let mut parts = key_val.splitn(2, ':');
                        let key = parts.next()?;
                        let value = parts.next().map_or(String::new(), String::from);
                        Some((String::from(key), value))
                    })
                    .collect::<HashMap<String, String>>()
            });

        Ok(UploadInfo {
            bytes_uploaded,
            total_size,
            metadata,
        })
    }

    /// Uploads a file to a given URL using the default chunk size.
    ///
    /// # Arguments
    ///
    /// * `url` - The URL to upload the file to.
    /// * `path` - The path of the file to be uploaded.
    ///
    /// # Returns
    ///
    /// A `Result` which is `Ok` if the file is successfully uploaded, otherwise `Err`.
    pub fn upload(&self, url: &str, path: &Path) -> Result<(), Error> {
        self.upload_with_chunk_size(url, path, DEFAULT_CHUNK_SIZE)
    }

    /// Uploads a file to a given URL in chunks of a specified size.
    ///
    /// # Arguments
    ///
    /// * `url` - The URL to upload the file to.
    /// * `path` - The path of the file to be uploaded.
    /// * `chunk_size` - The size of each chunk to be uploaded.
    ///
    /// # Returns
    ///
    /// A `Result` which is `Ok` if the file is successfully uploaded, otherwise `Err`.
    pub fn upload_with_chunk_size(
        &self,
        url: &str,
        path: &Path,
        chunk_size: usize,
    ) -> Result<(), Error> {
        let info = self.get_info(url)?;
        let file = File::open(path)?;
        let file_len = file.metadata()?.len();

        if let Some(total_size) = info.total_size {
            if file_len as usize != total_size {
                return Err(Error::UnequalSizeError);
            }
        }

        let mut reader = BufReader::new(&file);
        let mut buffer = vec![0; chunk_size];
        let mut progress = info.bytes_uploaded;

        let mut chunk_index = 0;
        // Checked first: resuming can find that the server already has it all.
        while progress < file_len as usize {
            // From the offset the server reports, which can be short of what was sent.
            reader.seek(SeekFrom::Start(progress as u64))?;

            let bytes_read = reader.read(&mut buffer)?;
            if bytes_read == 0 {
                return Err(Error::FileReadError);
            }

            print!("upload: chunk index: {}, ", chunk_index);

            let req = self.create_request(
                HttpMethod::Patch,
                url,
                Some(&buffer[..bytes_read]),
                Some(create_upload_headers(progress)),
            );

            let response = self.http_handler.deref().handle_request(req)?;

            if response.status_code == 409 {
                return Err(Error::WrongUploadOffsetError);
            }

            if response.status_code == 404 || response.status_code == 410 {
                return Err(Error::NotFoundError);
            }

            if response.status_code != 204 {
                return Err(Error::UnexpectedStatusCode(response.status_code));
            }

            let upload_offset = match response.headers.get_by_key(headers::UPLOAD_OFFSET) {
                Some(offset) => Ok(offset),
                None => Err(Error::MissingHeader(headers::UPLOAD_OFFSET.to_owned())),
            }?;

            let new_progress = upload_offset.parse()?;

            // Without this a server that accepts nothing would keep this loop going.
            if new_progress <= progress {
                return Err(Error::WrongUploadOffsetError);
            }
            progress = new_progress;

            chunk_index += 1;
        }

        Ok(())
    }

    /// Retrieves information about the server's Tus capabilities.
    ///
    /// # Arguments
    ///
    /// * `url` - The URL of the Tus server.
    ///
    /// # Returns
    ///
    /// A `Result` which is `Ok` if the server information is successfully retrieved, otherwise `Err`.    
    pub fn get_server_info(&self, url: &str) -> Result<ServerInfo, Error> {
        let req = self.create_request(HttpMethod::Options, url, None, None);

        let response = self.http_handler.deref().handle_request(req)?;

        if ![200_usize, 204].contains(&response.status_code) {
            return Err(Error::UnexpectedStatusCode(response.status_code));
        }

        let supported_versions = response
            .headers
            .get_by_key(headers::TUS_VERSION)
            .ok_or_else(|| Error::MissingHeader(headers::TUS_VERSION.to_owned()))?
            .split(',')
            .map(String::from)
            .collect::<Vec<String>>();

        let extensions = response
            .headers
            .get_by_key(headers::TUS_EXTENSION)
            .map_or_else(Vec::new, |ext| {
                ext.split(',')
                    .filter_map(|e| e.parse().ok())
                    .collect::<Vec<TusExtension>>()
            });

        let max_upload_size = response
            .headers
            .get_by_key(headers::TUS_MAX_SIZE)
            .and_then(|h| h.parse::<usize>().ok());

        Ok(ServerInfo {
            supported_versions,
            extensions,
            max_upload_size,
        })
    }

    /// Create a file on the server, receiving the upload URL of the file.
    pub fn create(&self, url: &str, path: &Path) -> Result<String, Error> {
        self.create_with_metadata(url, path, HashMap::new())
    }

    /// Creates a new upload with metadata on the Tus server.
    ///
    /// # Arguments
    ///
    /// * `url` - The URL of the Tus server.
    /// * `path` - The path of the file to be uploaded.
    /// * `metadata` - A map of metadata to be associated with the upload.
    ///
    /// # Returns
    ///
    /// A `Result` which is `Ok` if the upload is successfully created, otherwise `Err`.
    pub fn create_with_metadata(
        &self,
        url: &str,
        path: &Path,
        metadata: HashMap<String, String>,
    ) -> Result<String, Error> {
        let mut headers = default_headers();
        headers.insert(
            headers::UPLOAD_LENGTH.to_owned(),
            path.metadata()?.len().to_string(),
        );
        if !metadata.is_empty() {
            let data = metadata
                .iter()
                .map(|(key, value)| format!("{} {}", key, base64::encode(value)))
                .collect::<Vec<_>>()
                .join(",");
            headers.insert(headers::UPLOAD_METADATA.to_owned(), data);
        }

        let req = self.create_request(HttpMethod::Post, url, None, Some(headers));

        let response = self.http_handler.deref().handle_request(req)?;

        if response.status_code == 413 {
            return Err(Error::FileTooLarge);
        }

        if response.status_code != 201 {
            return Err(Error::UnexpectedStatusCode(response.status_code));
        }

        let location = response
            .headers
            .get_by_key(headers::LOCATION)
            .ok_or_else(|| Error::MissingHeader(headers::LOCATION.to_owned()))?;

        Ok(resolve_location(url, location))
    }

    /// Delete a file on the server.
    pub fn delete(&self, url: &str) -> Result<(), Error> {
        let req = self.create_request(HttpMethod::Delete, url, None, Some(default_headers()));

        let response = self.http_handler.deref().handle_request(req)?;

        if response.status_code != 204 {
            return Err(Error::UnexpectedStatusCode(response.status_code));
        }

        Ok(())
    }

    /// Creates an HTTP request with the specified method, URL, body, and headers.
    ///
    /// # Arguments
    ///
    /// * `method` - The HTTP method for the request.
    /// * `url` - The URL for the request.
    /// * `body` - The body of the request as a byte slice.
    /// * `headers` - The headers for the request.
    ///
    /// # Returns
    ///
    /// An `HttpRequest` object representing the created request.
    fn create_request<'b>(
        &self,
        method: HttpMethod,
        url: &str,
        body: Option<&'b [u8]>,
        headers: Option<Headers>,
    ) -> HttpRequest<'b> {
        let mut headers = headers.unwrap_or_default();

        if let Some(auth_token) = &self.auth_token {
            headers.insert("Authorization".to_owned(), format!("Bearer {}", auth_token));
            //println!("{}", format!("Bearer {}", auth_token));
        }

        let method = if self.use_method_override {
            headers.insert(
                headers::X_HTTP_METHOD_OVERRIDE.to_owned(),
                method.to_string(),
            );
            HttpMethod::Post
        } else {
            method
        };

        HttpRequest {
            method,
            url: String::from(url),
            body,
            headers,
        }
    }
}

/// Describes a file on the server.
#[derive(Debug)]
pub struct UploadInfo {
    /// How many bytes have been uploaded.
    pub bytes_uploaded: usize,
    /// The total size of the file.
    pub total_size: Option<usize>,
    /// Metadata supplied when the file was created.
    pub metadata: Option<HashMap<String, String>>,
}

/// Describes the tus enabled server.
#[derive(Debug)]
pub struct ServerInfo {
    /// The different versions of the tus protocol supported by the server, ordered by preference.
    pub supported_versions: Vec<String>,
    /// The extensions to the protocol supported by the server.
    pub extensions: Vec<TusExtension>,
    /// The maximum supported total size of a file.
    pub max_upload_size: Option<usize>,
}

/// Enumerates the extensions to the tus protocol.
#[derive(Debug, PartialEq)]
pub enum TusExtension {
    /// The server supports creating files.
    Creation,
    //// The server supports setting expiration time on files and uploads.
    Expiration,
    /// The server supports verifying checksums of uploaded chunks.
    Checksum,
    /// The server supports deleting files.
    Termination,
    /// The server supports parallel uploads of a single file.
    Concatenation,
}

impl FromStr for TusExtension {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "creation" => Ok(TusExtension::Creation),
            "expiration" => Ok(TusExtension::Expiration),
            "checksum" => Ok(TusExtension::Checksum),
            "termination" => Ok(TusExtension::Termination),
            "concatenation" => Ok(TusExtension::Concatenation),
            _ => Err(()),
        }
    }
}

/// Enumerates the errors which can occur during operation
#[derive(Debug)]
pub enum Error {
    /// The status code returned by the server was not one of the expected ones.
    UnexpectedStatusCode(usize),
    /// The file specified was not found by the server.
    NotFoundError,
    /// A required header was missing from the server response.
    MissingHeader(String),
    /// An error occurred while doing disk IO. This may be while reading a file, or during a network call.
    IoError(io::Error),
    /// Unable to parse a value, which should be an integer.
    ParsingError(ParseIntError),
    /// The size of the specified file, and the file size reported by the server do not match.
    UnequalSizeError,
    /// Unable to read the file specified.
    FileReadError,
    /// The `Client` tried to upload the file with an incorrect offset.
    WrongUploadOffsetError,
    /// The specified file is larger that what is supported by the server.
    FileTooLarge,
    /// An error occurred in the HTTP handler.
    HttpHandlerError(String),
}

/// Implements the `Display` trait for the `Error` enum.
///
/// This provides a human-readable description of the error, which can be used for error messages, logging, etc.
/// Each variant of the `Error` enum is mapped to a descriptive string.
impl Display for Error {
    fn fmt(&self, f: &mut Formatter) -> Result<(), std::fmt::Error> {
        let message = match self {
            Error::UnexpectedStatusCode(status_code) => format!("The status code returned by the server was not one of the expected ones: {}", status_code),
            Error::NotFoundError => "The file specified was not found by the server".to_string(),
            Error::MissingHeader(header_name) => format!("The '{}' header was missing from the server response", header_name),
            Error::IoError(error) => format!("An error occurred while doing disk IO. This may be while reading a file, or during a network call: {}", error),
            Error::ParsingError(error) => format!("Unable to parse a value, which should be an integer: {}", error),
            Error::UnequalSizeError => "The size of the specified file, and the file size reported by the server do not match".to_string(),
            Error::FileReadError => "Unable to read the specified file".to_string(),
            Error::WrongUploadOffsetError => "The client tried to upload the file with an incorrect offset".to_string(),
            Error::FileTooLarge => "The specified file is larger that what is supported by the server".to_string(),
            Error::HttpHandlerError(message) => format!("An error occurred in the HTTP handler: {}", message),
        };

        write!(f, "{}", message)?;

        Ok(())
    }
}

impl StdError for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::IoError(e)
    }
}

impl From<ParseIntError> for Error {
    fn from(e: ParseIntError) -> Self {
        Error::ParsingError(e)
    }
}

trait HeaderMap {
    fn get_by_key(&self, key: &str) -> Option<&String>;
}

impl HeaderMap for HashMap<String, String> {
    fn get_by_key(&self, key: &str) -> Option<&String> {
        self.keys()
            .find(|k| k.to_lowercase().as_str() == key)
            .and_then(|k| self.get(k))
    }
}

/// The tus specification allows a server to send a path as the location of an upload.
fn resolve_location(request_url: &str, location: &str) -> String {
    if !location.starts_with('/') {
        return location.to_owned();
    }
    let host_start = request_url.find("://").map_or(0, |i| i + 3);
    let host_end = request_url[host_start..]
        .find('/')
        .map_or(request_url.len(), |i| host_start + i);
    format!("{}{}", &request_url[..host_end], location)
}

/// Creates HTTP headers for an upload request, including the current progress.
///
/// # Arguments
///
/// * `progress` - The current progress of the upload.
///
/// # Returns
///
/// A `Headers` object containing the created headers.
fn create_upload_headers(progress: usize) -> Headers {
    let mut headers = default_headers();
    headers.insert(
        headers::CONTENT_TYPE.to_owned(),
        "application/offset+octet-stream".to_owned(),
    );
    headers.insert(headers::UPLOAD_OFFSET.to_owned(), progress.to_string());
    headers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::HttpResponse;
    use std::cell::RefCell;
    use std::io::Write;

    /// Answers from a script and records what it was sent.
    #[derive(Default)]
    struct Scripted {
        replies: RefCell<Vec<HttpResponse>>,
        sent: RefCell<Vec<(usize, Vec<u8>)>>,
    }

    fn reply(status_code: usize, headers: &[(&str, &str)]) -> HttpResponse {
        HttpResponse {
            status_code,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    impl HttpHandler for &Scripted {
        fn handle_request(&self, req: HttpRequest) -> Result<HttpResponse, Error> {
            if let Some(body) = req.body {
                let offset = req.headers[headers::UPLOAD_OFFSET].parse().unwrap();
                self.sent.borrow_mut().push((offset, body.to_vec()));
            }
            Ok(self.replies.borrow_mut().remove(0))
        }
    }

    fn temp_file(content: &[u8]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(content).unwrap();
        file
    }

    /// Reached when a retry finds the first attempt had in fact got everything there.
    #[test]
    fn test_an_upload_the_server_already_has_all_of_is_finished() {
        let script = Scripted::default();
        script.replies.borrow_mut().push(reply(
            200,
            &[("upload-offset", "5"), ("upload-length", "5")],
        ));
        let file = temp_file(b"12345");
        Client::new(&script)
            .upload_with_chunk_size("http://x/1", file.path(), 2)
            .unwrap();
        assert!(script.sent.borrow().is_empty());
    }

    #[test]
    fn test_an_empty_file_is_finished_as_soon_as_it_is_created() {
        let script = Scripted::default();
        script.replies.borrow_mut().push(reply(
            200,
            &[("upload-offset", "0"), ("upload-length", "0")],
        ));
        let file = temp_file(b"");
        Client::new(&script)
            .upload_with_chunk_size("http://x/1", file.path(), 2)
            .unwrap();
        assert!(script.sent.borrow().is_empty());
    }

    /// The next chunk must start where the server says it got to.
    #[test]
    fn test_chunks_follow_the_offset_the_server_reports() {
        let script = Scripted::default();
        *script.replies.borrow_mut() = vec![
            reply(200, &[("upload-offset", "0"), ("upload-length", "6")]),
            // Sent 4 bytes, and the server only kept 1.
            reply(204, &[("upload-offset", "1")]),
            reply(204, &[("upload-offset", "5")]),
            reply(204, &[("upload-offset", "6")]),
        ];
        let file = temp_file(b"abcdef");
        Client::new(&script)
            .upload_with_chunk_size("http://x/1", file.path(), 4)
            .unwrap();
        assert_eq!(
            *script.sent.borrow(),
            [
                (0, b"abcd".to_vec()),
                (1, b"bcde".to_vec()),
                (5, b"f".to_vec())
            ]
        );
    }

    #[test]
    fn test_a_chunk_that_gets_nowhere_is_an_error_not_an_endless_loop() {
        let script = Scripted::default();
        *script.replies.borrow_mut() = vec![
            reply(200, &[("upload-offset", "2"), ("upload-length", "4")]),
            reply(204, &[("upload-offset", "2")]),
        ];
        let file = temp_file(b"abcd");
        let result = Client::new(&script).upload_with_chunk_size("http://x/1", file.path(), 4);
        assert!(matches!(result, Err(Error::WrongUploadOffsetError)));
    }

    /// Only a 404 means the server has lost it. A 503 asks to try again, and
    /// treating it as lost would throw away everything uploaded so far.
    #[test]
    fn test_get_info_tells_an_outage_from_a_missing_upload() {
        for (status, is_not_found) in [(404, true), (410, true), (503, false), (401, false)] {
            let script = Scripted::default();
            script.replies.borrow_mut().push(reply(status, &[]));
            let error = Client::new(&script).get_info("http://x/1").unwrap_err();
            assert_eq!(
                matches!(error, Error::NotFoundError),
                is_not_found,
                "status {}",
                status
            );
            assert_eq!(
                matches!(error, Error::UnexpectedStatusCode(_)),
                !is_not_found,
                "status {}",
                status
            );
        }
    }

    #[test]
    fn test_get_info_without_an_offset_is_a_missing_header() {
        let script = Scripted::default();
        script.replies.borrow_mut().push(reply(200, &[]));
        let error = Client::new(&script).get_info("http://x/1").unwrap_err();
        assert!(matches!(error, Error::MissingHeader(_)));
    }

    /// A relative location is allowed by the tus specification.
    #[test]
    fn test_a_relative_location_is_resolved_against_the_server() {
        for (location, expected) in [
            ("/files/abc", "https://host:8080/files/abc"),
            ("https://other/files/abc", "https://other/files/abc"),
        ] {
            let script = Scripted::default();
            script
                .replies
                .borrow_mut()
                .push(reply(201, &[("location", location)]));
            let file = temp_file(b"x");
            let url = Client::new(&script)
                .create("https://host:8080/s5/upload/tus", file.path())
                .unwrap();
            assert_eq!(url, expected);
        }
    }
}
