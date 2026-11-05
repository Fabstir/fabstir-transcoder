use dotenv::var;
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use warp::{Filter, Rejection};

#[derive(Debug, Deserialize, Serialize)]
struct Claims {
    sub: String,
    exp: usize,
}

#[derive(Debug)]
struct InvalidToken;

impl warp::reject::Reject for InvalidToken {}

/// Compares every byte, so the time taken doesn't reveal how much of a guess is right.
fn tokens_match(given: &str, expected: &str) -> bool {
    let (given, expected) = (given.as_bytes(), expected.as_bytes());
    given.len() == expected.len()
        && given
            .iter()
            .zip(expected)
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

/// Creates a Warp filter for JWT authentication.
///
/// This function extracts the `Authorization` header from the incoming request,
/// verifies the JWT token, and ensures it matches the expected token stored in
/// the `FABSTIR_TRANSCODER_JWT` environment variable. It also decodes and validates
/// the token using the secret key stored in the `FABSTIR_TRANSCODER_SECRET_KEY`
/// environment variable.
///
/// # Returns
///
/// A Warp filter that verifies the JWT token and either continues the request
/// if the token is valid or rejects it with an `InvalidToken` rejection.
pub fn with_auth() -> impl Filter<Extract = (), Error = Rejection> + Clone {
    warp::header::<String>("authorization")
        .and_then(|header: String| async move {
            let token = header.strip_prefix("Bearer ").unwrap_or(&header);
            let env_token = match var("FABSTIR_TRANSCODER_JWT") {
                Ok(val) => val,
                Err(_) => return Err(warp::reject::custom(InvalidToken)),
            };

            if !tokens_match(token, &env_token) {
                return Err(warp::reject::custom(InvalidToken));
            }

            let key = match var("FABSTIR_TRANSCODER_SECRET_KEY") {
                Ok(val) => val,
                Err(_) => return Err(warp::reject::custom(InvalidToken)),
            };

            let validation = Validation::new(Algorithm::HS256);

            match decode::<Claims>(token, &DecodingKey::from_secret(key.as_ref()), &validation) {
                Ok(_) => Ok::<_, Rejection>(()), // Ensure the return type matches the expected type
                Err(_) => Err(warp::reject::custom(InvalidToken)),
            }
        })
        .untuple_one() // Flatten the nested tuple
}

/// Answers authentication failures with the 401 the API documents. Without this, warp
/// reports a custom rejection as a 500, and a missing header as a 400. Every other
/// rejection stays as warp reports it.
pub async fn handle_rejection(
    err: Rejection,
) -> Result<warp::reply::WithStatus<warp::reply::Json>, Rejection> {
    let missing_token = err
        .find::<warp::reject::MissingHeader>()
        .is_some_and(|missing| missing.name() == "authorization");
    if missing_token || err.find::<InvalidToken>().is_some() {
        let body = serde_json::json!({
            "status_code": 401,
            "message": "Invalid or missing token",
        });
        return Ok(warp::reply::with_status(
            warp::reply::json(&body),
            warp::http::StatusCode::UNAUTHORIZED,
        ));
    }
    Err(err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use warp::http::StatusCode;

    const SECRET: &str = "test-secret";

    fn token(exp: usize) -> String {
        let claims = Claims {
            sub: "node".to_string(),
            exp,
        };
        encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(SECRET.as_ref()),
        )
        .unwrap()
    }

    fn valid_token() -> &'static str {
        static TOKEN: once_cell::sync::Lazy<String> = once_cell::sync::Lazy::new(|| {
            let token = token(4_000_000_000);
            std::env::set_var("FABSTIR_TRANSCODER_JWT", &token);
            std::env::set_var("FABSTIR_TRANSCODER_SECRET_KEY", SECRET);
            token
        });
        &TOKEN
    }

    async fn status_for(authorization: Option<&str>) -> StatusCode {
        let route = warp::path!("x")
            .and(with_auth())
            .map(warp::reply)
            .recover(handle_rejection);
        let mut request = warp::test::request().path("/x");
        if let Some(value) = authorization {
            request = request.header("authorization", value);
        }
        request.reply(&route).await.status()
    }

    #[tokio::test]
    async fn test_the_right_token_is_accepted_with_or_without_its_prefix() {
        let token = valid_token();
        assert_eq!(
            status_for(Some(&format!("Bearer {}", token))).await,
            StatusCode::OK
        );
        assert_eq!(status_for(Some(token)).await, StatusCode::OK);
    }

    /// The API documents these as 401. They used to be a 500 and a 400, which reads
    /// as a server fault and can't be told from one by a client deciding to retry.
    #[tokio::test]
    async fn test_a_missing_or_wrong_token_is_a_401() {
        let token = valid_token();
        for wrong in ["Bearer wrong", "wrong", "Bearer ", ""] {
            assert_eq!(
                status_for(Some(wrong)).await,
                StatusCode::UNAUTHORIZED,
                "{:?}",
                wrong
            );
        }
        assert_eq!(status_for(None).await, StatusCode::UNAUTHORIZED);
        // Only the one prefix comes off.
        assert_eq!(
            status_for(Some(&format!("Bearer Bearer {}", token))).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn test_tokens_compare_equal_only_when_they_are_equal() {
        assert!(tokens_match("abc", "abc"));
        assert!(tokens_match("", ""));
        for (a, b) in [("abc", "abd"), ("abc", "abcd"), ("abcd", "abc"), ("", "a")] {
            assert!(!tokens_match(a, b), "{} {}", a, b);
        }
    }

    /// Anything else stays as warp reports it.
    #[tokio::test]
    async fn test_other_rejections_are_left_alone() {
        let route = warp::path!("x")
            .and(warp::get())
            .map(warp::reply)
            .recover(handle_rejection);
        let not_found = warp::test::request().path("/y").reply(&route).await;
        assert_eq!(not_found.status(), StatusCode::NOT_FOUND);
    }
}
