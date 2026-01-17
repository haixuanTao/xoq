//! iroh client example using the wser library API

use anyhow::Result;
use std::env;
use std::time::Duration;
use tokio::time;
use wser::IrohClientBuilder;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    // Get server endpoint ID from command line
    let server_id = env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("Usage: client_iroh <server-endpoint-id>"))?;

    tracing::info!("Connecting to server: {}", server_id);

    // Connect to server
    let conn = IrohClientBuilder::new()
        .connect_str(&server_id)
        .await?;

    tracing::info!("Connected to server!");

    // Open a stream
    let stream = conn.open_stream().await?;

    // Spawn receiver task
    let (send, mut recv) = stream.split();

    tokio::spawn(async move {
        let mut buf = vec![0u8; 4096];
        loop {
            match recv.read(&mut buf).await {
                Ok(Some(n)) => {
                    let msg = String::from_utf8_lossy(&buf[..n]);
                    tracing::info!("Received: {}", msg.trim());
                }
                Ok(None) => {
                    tracing::info!("Stream closed by server");
                    break;
                }
                Err(e) => {
                    tracing::error!("Read error: {}", e);
                    break;
                }
            }
        }
    });

    // Send messages
    let mut send = send;
    let mut counter = 0u64;
    loop {
        let msg = format!("Hello from client #{}\n", counter);
        tracing::info!("Sending: {}", msg.trim());
        send.write_all(msg.as_bytes()).await?;

        counter += 1;
        time::sleep(Duration::from_secs(2)).await;
    }
}
