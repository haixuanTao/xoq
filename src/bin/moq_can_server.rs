//! MoQ CAN bridge server - bridges local CAN interface to remote clients via MoQ relay
//!
//! Broadcasts CAN state to all subscribers (1-to-many) and accepts commands from
//! any publisher (many-to-1, last command wins).
//!
//! Usage: moq_can_server <interface[:fd]> [moq_path] [--relay <url>] [--insecure]
//!
//! Examples:
//!   moq_can_server can0                           # Standard CAN, default path
//!   moq_can_server can0:fd                        # CAN FD
//!   moq_can_server can0 anon/my-robot             # Custom path
//!   moq_can_server can0 --relay https://my.relay  # Custom relay
//!   moq_can_server can0 --relay https://172.18.133.111:4443 --insecure  # Self-hosted relay

use anyhow::Result;
use std::env;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("xoq=info".parse()?)
                .add_directive("warn".parse()?),
        )
        .init();

    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        print_usage();
        return Ok(());
    }

    let mut relay = "https://cdn.moq.dev".to_string();
    let mut interface_arg: Option<String> = None;
    let mut moq_path: Option<String> = None;
    let mut insecure = false;

    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--relay" {
            if i + 1 < args.len() {
                relay = args[i + 1].clone();
                i += 2;
                continue;
            } else {
                eprintln!("Error: --relay requires a URL argument");
                return Ok(());
            }
        }
        if arg == "--insecure" {
            insecure = true;
            i += 1;
            continue;
        }
        if interface_arg.is_none() {
            interface_arg = Some(arg.clone());
        } else if moq_path.is_none() {
            moq_path = Some(arg.clone());
        }
        i += 1;
    }

    let interface_arg = match interface_arg {
        Some(a) => a,
        None => {
            print_usage();
            return Ok(());
        }
    };

    let (interface, enable_fd) = if let Some(iface) = interface_arg.strip_suffix(":fd") {
        (iface.to_string(), true)
    } else {
        (interface_arg.clone(), false)
    };

    let moq_path = moq_path.unwrap_or_else(|| format!("anon/xoq-can-{}", interface));

    let mut server = xoq::MoqCanServer::new(&interface, enable_fd)?;

    tracing::info!("Interface: {} (FD: {})", server.interface(), server.is_fd());
    tracing::info!(
        "MoQ relay: {}{}",
        relay,
        if insecure { " (insecure)" } else { "" }
    );
    tracing::info!("MoQ state broadcast: {}/state", moq_path);
    tracing::info!("MoQ command listener: {}/commands", moq_path);

    server.run(&relay, &moq_path, insecure).await?;

    Ok(())
}

fn print_usage() {
    println!("Usage: moq_can_server <interface[:fd]> [moq_path] [--relay <url>] [--insecure]");
    println!();
    println!("Broadcasts CAN state to all subscribers and accepts commands from any publisher.");
    println!("  State:    {{path}}/state    (track: can) — 1-to-many fan-out");
    println!("  Commands: {{path}}/commands (track: can) — many-to-1, last command wins");
    println!();
    println!("Examples:");
    println!("  moq_can_server can0                           # Standard CAN");
    println!("  moq_can_server can0:fd                        # CAN FD");
    println!("  moq_can_server can0 anon/my-robot             # Custom MoQ path");
    println!("  moq_can_server can0 --relay https://my.relay  # Custom relay");
    println!("  moq_can_server can0 --relay https://172.18.133.111:4443 --insecure");
    println!();
    println!("Options:");
    println!("  :fd                 Append to interface name to enable CAN FD");
    println!("  --relay <url>       MoQ relay URL (default: https://cdn.moq.dev)");
    println!("  --insecure          Disable TLS verification (for self-signed certs)");
    println!();
    println!("Available CAN interfaces:");
    match xoq::list_interfaces() {
        Ok(interfaces) => {
            if interfaces.is_empty() {
                println!("  (none found)");
                println!();
                println!("To create a virtual CAN interface for testing:");
                println!("  sudo modprobe vcan");
                println!("  sudo ip link add dev vcan0 type vcan");
                println!("  sudo ip link set up vcan0");
            } else {
                for iface in interfaces {
                    println!("  {}", iface.name);
                }
            }
        }
        Err(e) => println!("  Error listing interfaces: {}", e),
    }
}
