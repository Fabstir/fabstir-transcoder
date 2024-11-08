use jsonwebtoken::{decode, DecodingKey, Validation, Algorithm};
use serde::{Deserialize, Serialize};
use warp::reject::custom;
use warp::{Filter, Rejection};
use dotenv::var;

#[derive(Debug, Deserialize, Serialize)]
struct Claims {
    sub: String,
    exp: usize,
}

#[derive(Debug)]
struct InvalidToken;

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
impl warp::reject::Reject for InvalidToken {}

pub fn with_auth() -> impl Filter<Extract = (), Error = Rejection> + Clone {
    warp::header::<String>("authorization")
        .and_then(|token: String| async move {
            let token = token.trim_start_matches("Bearer ");
            let env_token = match var("FABSTIR_TRANSCODER_JWT") {
                Ok(val) => val,
                Err(_) => return Err(warp::reject::custom(InvalidToken)),
            };

            if token != env_token {
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