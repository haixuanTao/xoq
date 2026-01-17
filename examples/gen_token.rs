//! Token generation example for MoQ authentication
//! Generates JWT tokens for publish/subscribe access

use anyhow::Result;
use wser::moq_token::{Algorithm, Claims, Key};
use std::time::{Duration, SystemTime};

fn main() -> Result<()> {
    // Generate a new key pair (in production, load from file)
    let key = Key::generate(Algorithm::ES256, Some("wser-key".to_string()))?;

    // Save the key for later use (server needs this to verify)
    println!("=== Generated Key (save this) ===");
    println!("{}", key.to_str()?);
    println!();

    // Create claims for a publisher (can publish and subscribe)
    let mut pub_claims = Claims::default();
    pub_claims.root = "wser-auth".to_string();
    pub_claims.publish = vec!["".to_string()]; // Can publish to root
    pub_claims.subscribe = vec!["".to_string()]; // Can subscribe to root
    pub_claims.expires = Some(SystemTime::now() + Duration::from_secs(3600)); // 1 hour
    pub_claims.issued = Some(SystemTime::now());

    let pub_token = key.encode(&pub_claims)?;
    println!("=== Publisher Token (1 hour expiry) ===");
    println!("{}", pub_token);
    println!();

    // Create claims for a subscriber (read-only)
    let mut sub_claims = Claims::default();
    sub_claims.root = "wser-auth".to_string();
    sub_claims.subscribe = vec!["".to_string()]; // Can only subscribe
    sub_claims.expires = Some(SystemTime::now() + Duration::from_secs(3600));
    sub_claims.issued = Some(SystemTime::now());

    let sub_token = key.encode(&sub_claims)?;
    println!("=== Subscriber Token (read-only, 1 hour expiry) ===");
    println!("{}", sub_token);
    println!();

    // Show how to use it
    println!("=== Usage ===");
    println!("Server URL: https://your-relay.example.com/wser-auth?token={}", pub_token);
    println!();

    // Verify the token works
    let decoded = key.decode(&pub_token)?;
    println!("=== Verified Claims ===");
    println!("Root: {}", decoded.root);
    println!("Publish paths: {:?}", decoded.publish);
    println!("Subscribe paths: {:?}", decoded.subscribe);

    Ok(())
}
