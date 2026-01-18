//! Serial port bridge client - connects to remote serial port
//!
//! Usage: serial_client <server-endpoint-id>
//!
//! Set RUST_LOG=debug for verbose output

use anyhow::Result;
use std::env;
use xoq::Client;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("xoq=debug".parse()?)
                .add_directive("info".parse()?),
        )
        .init();

    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        println!("Usage: serial_client <server-endpoint-id>");
        return Ok(());
    }

    let server_id = &args[1];
    println!("Connecting to serial bridge: {}", server_id);

    // Connect to the remote serial port
    let client = Client::connect(server_id).await?;

    println!("Connected! Starting interactive terminal...");
    println!("Type to send, Ctrl+C to exit.\n");

    // Run interactive terminal - all I/O handled internally
    client.run_interactive().await?;

    Ok(())
}
