//! Authenticated MoQ client example using the wser library API

use anyhow::Result;
use std::env;
use std::time::Duration;
use tokio::time;
use wser::MoqBuilder;

#[tokio::main]
async fn main() -> Result<()> {
    moq_native::Log {
        level: tracing::Level::INFO,
    }
    .init();

    // Get token from command line or environment
    let token = env::args()
        .nth(1)
        .or_else(|| env::var("MOQ_TOKEN").ok())
        .ok_or_else(|| anyhow::anyhow!("Usage: client_auth <token> or set MOQ_TOKEN env var"))?;

    tracing::info!("Connecting to relay with auth...");

    // Connect with token authentication
    let mut conn = MoqBuilder::new()
        .path("wser-auth")
        .token(&token)
        .connect_duplex()
        .await?;

    tracing::info!("Connected and authenticated");

    // Create a track for publishing
    let mut track = conn.create_track("client-messages");
    tracing::info!("Publishing on 'client-messages' track");

    // Spawn subscriber task
    let mut sub_origin = conn.subscribe_origin().clone();
    tokio::spawn(async move {
        tracing::info!("Waiting for server broadcasts...");
        while let Some((path, broadcast)) = sub_origin.announced().await {
            if let Some(broadcast) = broadcast {
                tracing::info!("Server broadcast announced: {}", path);
                let track_info = moq_native::moq_lite::Track {
                    name: "server-messages".to_string(),
                    priority: 0,
                };
                let mut track = broadcast.subscribe_track(&track_info);

                tokio::spawn(async move {
                    while let Ok(Some(mut group)) = track.next_group().await {
                        while let Ok(Some(frame)) = group.read_frame().await {
                            let msg = String::from_utf8_lossy(&frame);
                            tracing::info!("Received from server: {}", msg);
                        }
                    }
                });
            }
        }
    });

    // Publish messages
    let mut counter = 0u64;
    loop {
        let msg = format!("Client message #{}", counter);
        tracing::info!("Sending: {}", msg);
        track.write_str(&msg);

        counter += 1;
        time::sleep(Duration::from_secs(3)).await;
    }
}
