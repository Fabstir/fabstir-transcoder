use crate::http::{HttpHandler, HttpMethod, HttpRequest, HttpResponse};
use crate::Error;
use reqwest::header::{HeaderMap, HeaderName};
use reqwest::Method;
use std::collections::HashMap;
use std::str::FromStr;

impl HttpHandler for reqwest::blocking::Client {
    fn handle_request(&self, req: HttpRequest) -> Result<HttpResponse, Error> {
        let mut headers = HeaderMap::new();
        for (key, value) in req.headers {
            // Not including the value in the error: this could be the token.
            let name = HeaderName::from_str(&key)
                .map_err(|_| Error::HttpHandlerError(format!("unusable header name: {}", key)))?;
            let value = value
                .parse()
                .map_err(|_| Error::HttpHandlerError(format!("unusable value for {}", key)))?;
            headers.insert(name, value);
        }

        let mut builder = match req.method {
            HttpMethod::Head => self.head(&req.url),
            HttpMethod::Patch => self.patch(&req.url),
            HttpMethod::Options => self.request(Method::OPTIONS, &req.url),
            HttpMethod::Post => self.post(&req.url),
            HttpMethod::Delete => self.delete(&req.url),
        }
        .headers(headers);

        if let Some(body) = req.body {
            builder = builder.body(Vec::from(body));
        }

        let response = match builder.send() {
            Ok(resp) => resp,
            Err(err) => return Err(Error::HttpHandlerError(err.to_string())),
        };

        let mut headers = HashMap::new();
        for (key, value) in response.headers() {
            headers.insert(
                key.to_string(),
                value.to_str().map(String::from).unwrap_or_default(),
            );
        }

        Ok(HttpResponse {
            status_code: response.status().as_u16() as usize,
            headers,
        })
    }
}

#[cfg(test)]
mod tests {
    /// A token read from a file often ends in a newline, which can't go in a header.
    /// That used to panic on the upload thread instead of failing the upload.
    #[test]
    fn test_an_unusable_header_is_an_error() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let client =
            crate::Client::new(reqwest::blocking::Client::new()).with_auth_token("token\n");

        let result = client.create("http://127.0.0.1:1/", file.path());

        match result {
            Err(crate::Error::HttpHandlerError(message)) => {
                assert!(
                    !message.contains("token\n"),
                    "the value must not be repeated"
                );
            }
            other => panic!("expected a handler error, got {:?}", other),
        }
    }
}
