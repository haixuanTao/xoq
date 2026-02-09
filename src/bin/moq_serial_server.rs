//! MoQ serial bridge server
//!
//! Usage: moq_serial_server <port> [baud_rate] [relay_url] [moq_path]
//! Example: moq_serial_server /dev/ttyUSB0 1000000 https://172.18.133.111:4443 anon/xoq-serial

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
        println!("Usage: moq_serial_server <port> [baud_rate] [relay_url] [moq_path]");
        println!("Example: moq_serial_server /dev/ttyUSB0 1000000 https://172.18.133.111:4443 anon/xoq-serial");
        println!("\nAvailable ports:");
        for port in xoq::list_ports()? {
            println!("  {} - {:?}", port.name, port.port_type);
        }
        return Ok(());
    }

    let port_name = &args[1];
    let baud_rate: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1000000);
    let relay_url = args
        .get(3)
        .cloned()
        .unwrap_or_else(|| "https://172.18.133.111:4443".to_string());
    let moq_path = args
        .get(4)
        .cloned()
        .unwrap_or_else(|| "anon/xoq-serial".to_string());

    // Open serial port
    let serial = xoq::serial::SerialPort::open_simple(port_name, baud_rate)?;
    let (mut reader, mut writer) = serial.split();

    tracing::info!("Serial port opened: {} @ {} baud", port_name, baud_rate);

    // Connect to MoQ relay using MoqStream (server side)
    tracing::info!("Connecting to {} at path '{}'...", relay_url, moq_path);
    let stream = xoq::MoqStream::accept_at_insecure(&relay_url, &moq_path).await?;
    let stream = Arc::new(Mutex::new(stream));
    tracing::info!("Connected to MoQ relay");

    // Spawn task: network -> serial
    let stream_read = stream.clone();
    let net_to_serial = tokio::spawn(async move {
        let mut last_recv = Instant::now();
        loop {
            let data = {
                let mut s = stream_read.lock().await;
                s.read().await
            };
            match data {
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
                let mut s = stream.lock().await;
                s.write(buf[..n].to_vec());
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
