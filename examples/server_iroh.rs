//! iroh server example using the wser library API

use anyhow::Result;
use wser::IrohServerBuilder;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    // Build server with persistent identity
    let server = IrohServerBuilder::new()
        .identity_path(".wser_server_key")
        .bind()
        .await?;

    tracing::info!("Server started");
    tracing::info!("Endpoint ID (give this to client): {}", server.id());

    // Accept connections
    loop {
        tracing::info!("Waiting for connections...");

        if let Some(conn) = server.accept().await? {
            tracing::info!("Client connected: {}", conn.remote_id());

            tokio::spawn(async move {
                if let Err(e) = handle_client(conn).await {
                    tracing::error!("Client error: {}", e);
                }
            });
        }
    }
}

async fn handle_client(conn: wser::IrohConnection) -> Result<()> {
    loop {
        let mut stream = conn.accept_stream().await?;
        tracing::info!("New stream opened");

        tokio::spawn(async move {
            loop {
                match stream.read_string().await {
                    Ok(Some(msg)) => {
                        tracing::info!("Received: {}", msg.trim());

                        // Echo back
                        let response = format!("Server echo: {}", msg);
                        if stream.write_str(&response).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => {
                        tracing::info!("Stream closed");
                        break;
                    }
                    Err(e) => {
                        tracing::error!("Read error: {}", e);
                        break;
                    }
                }
            }
        });
    }
}
