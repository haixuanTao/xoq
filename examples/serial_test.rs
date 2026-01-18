//! Simple serial port test - bypasses network to test serial directly
//!
//! Usage: serial_test <port> [baud_rate]

use anyhow::Result;
use std::env;
use std::io::Write;
use xoq::SerialPort;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("debug")
        .init();

    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        println!("Usage: serial_test <port> [baud_rate]");
        println!("\nAvailable ports:");
        for port in xoq::list_ports()? {
            println!("  {} - {:?}", port.name, port.port_type);
        }
        return Ok(());
    }

    let port_name = &args[1];
    let baud_rate: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(115200);

    println!("Opening {} @ {} baud...", port_name, baud_rate);
    let port = SerialPort::open_simple(port_name, baud_rate)?;
    println!("Port opened successfully!");

    println!("\nType commands to send to serial port. Responses will be shown.");
    println!("Press Ctrl+C to exit.\n");

    // Split into reader/writer
    let (mut reader, mut writer) = port.split();

    // Spawn task to read from serial and print
    let read_task = tokio::spawn(async move {
        let mut buf = [0u8; 1024];
        let mut total_reads = 0u64;
        eprintln!("[Read task started]");
        loop {
            total_reads += 1;

            match reader.read(&mut buf).await {
                Ok(n) if n > 0 => {
                    eprintln!("[Received {} bytes]", n);
                    let data = &buf[..n];
                    // Print as hex for binary data
                    eprint!("  Hex: ");
                    for b in data {
                        eprint!("{:02x} ", b);
                    }
                    eprintln!();
                    // Also try as string
                    eprintln!("  Str: {}", String::from_utf8_lossy(data));
                    std::io::stdout().flush().ok();
                }
                Ok(_) => {
                    // 0 bytes - timeout, normal
                    if total_reads % 100 == 0 {
                        eprintln!("[Polling... {} attempts]", total_reads);
                    }
                }
                Err(e) => {
                    eprintln!("[Serial read error: {}]", e);
                    break;
                }
            }
        }
    });

    // Main task: read stdin and write to serial
    loop {
        let input = tokio::task::spawn_blocking(|| {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).ok();
            line
        }).await?;

        if input.is_empty() {
            break;
        }

        eprintln!("[Sending {} bytes: {:?}]", input.len(), input.as_bytes());
        writer.write_all(input.as_bytes()).await?;
        eprintln!("[Sent and flushed]");
    }

    read_task.abort();
    Ok(())
}
