//! CAN bridge server - bridges local CAN interface to remote clients
//!
//! Usage: can_server <interface[:fd]>... [--key-dir <path>] [--moq-relay <url>] [--moq-path <path>] [--moq-insecure]
//!
//! Examples:
//!   can_server can0                      # Single interface (backward compatible)
//!   can_server can0:fd                   # Single interface with CAN FD
//!   can_server can0 can1 vcan0           # Multiple interfaces
//!   can_server can0:fd can1              # Mixed (can0=FD, can1=standard)
//!   can_server can0 --key-dir /etc/xoq   # Custom key directory
//!   can_server can0:fd --moq-relay https://cdn.1ms.ai                        # iroh + MoQ
//!   can_server can0:fd --moq-relay https://cdn.1ms.ai --moq-path anon/my-robot
//!   can_server can0:fd can1:fd --moq-relay https://cdn.1ms.ai                # multi-interface + MoQ

use anyhow::Result;
use std::env;
use std::path::PathBuf;
use std::time::Duration;
use tokio::task::JoinSet;
use xoq::CanServer;

/// Configuration for a single CAN interface server
struct InterfaceConfig {
    interface: String,
    enable_fd: bool,
    identity_path: PathBuf,
    moq_relay: Option<String>,
    moq_path: Option<String>,
    moq_insecure: bool,
}

/// Parse command line arguments
fn parse_args() -> Option<(Vec<InterfaceConfig>, PathBuf)> {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        return None;
    }

    let mut interfaces = Vec::new();
    let mut key_dir = PathBuf::from(".");
    let mut moq_relay: Option<String> = None;
    let mut moq_path: Option<String> = None;
    let mut moq_insecure = false;
    let mut i = 1;

    while i < args.len() {
        let arg = &args[i];

        if arg == "--key-dir" {
            if i + 1 < args.len() {
                key_dir = PathBuf::from(&args[i + 1]);
                i += 2;
                continue;
            } else {
                eprintln!("Error: --key-dir requires a path argument");
                return None;
            }
        }

        if arg == "--moq-relay" {
            if i + 1 < args.len() {
                moq_relay = Some(args[i + 1].clone());
                i += 2;
                continue;
            } else {
                eprintln!("Error: --moq-relay requires a URL argument");
                return None;
            }
        }

        if arg == "--moq-path" {
            if i + 1 < args.len() {
                moq_path = Some(args[i + 1].clone());
                i += 2;
                continue;
            } else {
                eprintln!("Error: --moq-path requires a path argument");
                return None;
            }
        }

        if arg == "--moq-insecure" {
            moq_insecure = true;
            i += 1;
            continue;
        }

        // Skip legacy flags
        if arg == "--no-jitter-buffer" || arg == "--fd" {
            i += 1;
            continue;
        }

        // Parse interface with optional :fd suffix
        let (interface, enable_fd) = if let Some(iface) = arg.strip_suffix(":fd") {
            (iface.to_string(), true)
        } else {
            (arg.clone(), false)
        };

        interfaces.push((interface, enable_fd));
        i += 1;
    }

    if interfaces.is_empty() {
        return None;
    }

    // Build configs with per-interface identity paths
    let configs = interfaces
        .into_iter()
        .map(|(interface, enable_fd)| {
            let identity_path = key_dir.join(format!(".xoq_can_server_key_{}", interface));
            InterfaceConfig {
                interface,
                enable_fd,
                identity_path,
                moq_relay: moq_relay.clone(),
                moq_path: moq_path.clone(),
                moq_insecure,
            }
        })
        .collect();

    Some((configs, key_dir))
}

fn print_usage() {
    println!("Usage: can_server <interface[:fd]>... [--key-dir <path>] [--moq-relay <url>] [--moq-path <path>] [--moq-insecure]");
    println!();
    println!("Examples:");
    println!("  can_server can0                      # Single interface");
    println!("  can_server can0:fd                   # Single interface with CAN FD");
    println!("  can_server can0 can1 vcan0           # Multiple interfaces");
    println!("  can_server can0:fd can1              # Mixed (can0=FD, can1=standard)");
    println!("  can_server can0 --key-dir /etc/xoq   # Custom key directory");
    println!(
        "  can_server can0:fd --moq-relay https://cdn.1ms.ai                          # iroh + MoQ"
    );
    println!("  can_server can0:fd --moq-relay https://cdn.1ms.ai --moq-path anon/my-robot # custom path");
    println!("  can_server can0:fd can1:fd --moq-relay https://cdn.1ms.ai                  # multi + MoQ");
    println!();
    println!("Options:");
    println!("  :fd                  Append to interface name to enable CAN FD");
    println!("  --key-dir <path>     Directory for identity key files (default: current dir)");
    println!("  --moq-relay <url>    Also broadcast CAN state via MoQ relay for browsers");
    println!("  --moq-path <path>    MoQ broadcast path (default: anon/xoq-can-<interface>)");
    println!("  --moq-insecure       Disable TLS verification (for self-signed certs)");
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

/// Run a single CAN server with auto-restart on failure
async fn run_server_supervised(config: InterfaceConfig) {
    let interface = config.interface.clone();

    loop {
        tracing::info!("[{}] Starting server...", interface);

        match run_server(&config).await {
            Ok(()) => {
                tracing::info!("[{}] Server stopped cleanly", interface);
                break;
            }
            Err(e) => {
                tracing::error!("[{}] Server failed: {}", interface, e);
                tracing::info!("[{}] Restarting in 5 seconds...", interface);
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

/// Run a single CAN server
async fn run_server(config: &InterfaceConfig) -> Result<()> {
    let identity_path_str = config.identity_path.to_string_lossy();
    let server = CanServer::new(
        &config.interface,
        config.enable_fd,
        Some(&identity_path_str),
        config.moq_relay.as_deref(),
        config.moq_path.as_deref(),
        config.moq_insecure,
    )
    .await?;

    tracing::info!(
        "[{}] Interface: {} (FD: {})",
        config.interface,
        config.interface,
        config.enable_fd,
    );
    tracing::info!("[{}] Server ID: {}", config.interface, server.id());
    tracing::info!(
        "[{}] Identity: {}",
        config.interface,
        config.identity_path.display()
    );
    if let Some(ref relay) = config.moq_relay {
        tracing::info!(
            "[{}] MoQ relay: {}{}",
            config.interface,
            relay,
            if config.moq_insecure {
                " (insecure)"
            } else {
                ""
            }
        );
    }
    tracing::info!("[{}] Waiting for connections...", config.interface);

    server.run().await?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("xoq=info".parse()?)
                .add_directive("warn".parse()?),
        )
        .init();

    let (configs, key_dir) = match parse_args() {
        Some(result) => result,
        None => {
            print_usage();
            return Ok(());
        }
    };

    tracing::info!("Starting CAN bridge server");
    tracing::info!("Key directory: {}", key_dir.display());
    tracing::info!("Interfaces: {}", configs.len());

    // Spawn all servers into a JoinSet
    let mut servers: JoinSet<()> = JoinSet::new();

    for config in configs {
        servers.spawn(run_server_supervised(config));
    }

    // Wait for Ctrl+C or all servers to exit
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Received Ctrl+C, shutting down all servers...");
            servers.abort_all();
            // Give writer threads time to notice channel closure and print final stats
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        _ = async {
            while servers.join_next().await.is_some() {}
        } => {
            tracing::info!("All servers have stopped");
        }
    }

    Ok(())
}
