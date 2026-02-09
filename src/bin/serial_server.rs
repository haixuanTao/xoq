//! Serial port bridge server - all forwarding handled internally
//!
//! Usage: serial_server <port> [baud_rate] [--moq [moq_path]]
//! Example: serial_server /dev/ttyUSB0 115200
//! Example: serial_server /dev/ttyUSB0 1000000 --moq anon/xoq-test

use anyhow::Result;
use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

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
        println!("Usage: serial_server <port> [baud_rate] [--moq [moq_path]]");
        println!("Example (iroh): serial_server /dev/ttyUSB0 115200");
        println!("Example (moq):  serial_server /dev/ttyUSB0 1000000 --moq anon/xoq-test");
        println!("\nAvailable ports:");
        for port in xoq::list_ports()? {
            println!("  {} - {:?}", port.name, port.port_type);
        }
        return Ok(());
    }

    let port_name = &args[1];
    let baud_rate: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(115200);

    // Check for --moq flag and optional moq_path
    let moq_idx = args.iter().position(|a| a == "--moq");
    if let Some(idx) = moq_idx {
        let moq_path = args
            .get(idx + 1)
            .cloned()
            .unwrap_or_else(|| "anon/xoq-serial".to_string());
        run_moq_server(port_name, baud_rate, &moq_path).await
    } else {
        run_iroh_server(port_name, baud_rate).await
    }
}

async fn run_iroh_server(port_name: &str, baud_rate: u32) -> Result<()> {
    let bridge = xoq::Server::new(port_name, baud_rate, Some(".xoq_server_key")).await?;

    tracing::info!("Serial bridge server started (iroh P2P)");
    tracing::info!("Port: {} @ {} baud", port_name, baud_rate);
    tracing::info!("Server ID: {}", bridge.id());
    tracing::info!("Waiting for connections...");

    bridge.run().await?;
    Ok(())
}

async fn run_moq_server(port_name: &str, baud_rate: u32, moq_path: &str) -> Result<()> {
    // Open serial port
    let serial = xoq::serial::SerialPort::open_simple(port_name, baud_rate)?;
    let (mut reader, mut writer) = serial.split();

    tracing::info!("Serial port opened: {} @ {} baud", port_name, baud_rate);

    // Connect via MoqStream (waits for client, no timeout)
    let relay = "https://cdn.moq.dev";
    tracing::info!(
        "Connecting to MoQ relay at '{}' path '{}'...",
        relay,
        moq_path
    );
    let stream = xoq::MoqStream::accept_at(relay, moq_path).await?;
    let stream = Arc::new(Mutex::new(stream));
    tracing::info!("Client connected!");

    // Spawn task: network -> serial
    let stream_read = stream.clone();
    let net_to_serial = tokio::spawn(async move {
        let mut last_recv = Instant::now();
        loop {
            let read_result = {
                let mut stream = stream_read.lock().await;
                stream.read().await
            };
            match read_result {
                Ok(Some(data)) => {
                    let gap = last_recv.elapsed();
                    if gap > Duration::from_millis(50) {
                        tracing::warn!(
                            "MoQ Net→Serial: {:.1}ms gap ({} bytes)",
                            gap.as_secs_f64() * 1000.0,
                            data.len(),
                        );
                    }
                    last_recv = Instant::now();
                    tracing::debug!("MoQ Net→Serial: {} bytes", data.len());
                    if let Err(e) = writer.write_all(&data).await {
                        tracing::error!("Serial write error: {}", e);
                        break;
                    }
                }
                Ok(None) => {
                    tracing::info!("MoQ stream ended");
                    break;
                }
                Err(e) => {
                    tracing::error!("MoQ read error: {}", e);
                    break;
                }
            }
        }
    });

    // Main task: serial -> network
    let mut buf = [0u8; 1024];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => {
                tokio::task::yield_now().await;
            }
            Ok(n) => {
                tracing::debug!("Serial→MoQ: {} bytes", n);
                let mut stream = stream.lock().await;
                stream.write(buf[..n].to_vec());
            }
            Err(e) => {
                tracing::error!("Serial read error: {}", e);
                break;
            }
        }
    }

    net_to_serial.abort();
    Ok(())
}
