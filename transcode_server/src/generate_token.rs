use jsonwebtoken::{encode, Header, EncodingKey};
use serde::{Serialize, Deserialize};
use std::env;
use dotenv::dotenv;

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    sub: String,
    exp: usize,
}

/// Generates a JWT token using the secret key from the environment variable
/// `FABSTIR_TRANSCODER_SECRET_KEY` and prints the generated token.
fn main() {
    // Load environment variables from .env file
    dotenv().ok();

    // Print all environment variables for debugging
    for (key, value) in env::vars() {
        println!("{}: {}", key, value);
    }

    // Retrieve the secret key from the environment variable
    let secret_key = env::var("FABSTIR_TRANSCODER_SECRET_KEY").expect("FABSTIR_TRANSCODER_SECRET_KEY must be set");

    // Set the claims for the token
    let claims = Claims {
        sub: "user_id".to_string(),
        exp: 10000000000, // Set an appropriate expiration time
    };

    // Encode the token
    let token = encode(&Header::default(), &claims, &EncodingKey::from_secret(secret_key.as_ref())).unwrap();

    // Print the token
    println!("Generated JWT token: {}", token);
}