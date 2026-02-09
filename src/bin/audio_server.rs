//! Audio server - bridges local mic/speaker to remote clients over P2P or MoQ
//!
//! Usage: audio_server [options]
//!
//! Examples:
//!   audio_server                            # Default mic, iroh P2P
//!   audio_server --input 1 --output 0       # Specific devices
//!   audio_server --sample-rate 44100        # Custom sample rate
//!   audio_server --moq anon/my-audio        # MoQ relay transport
//!   audio_server --no-vpio                   # Disable VPIO, use cpal backend
//!   audio_server --list                     # List available devices

use anyhow::Result;
use xoq::audio::{AudioDevice, SampleFormat};
use xoq::audio_server::AudioServerBuilder;

fn print_usage() {
    println!("Usage: audio_server [options]");
    println!();
    println!("Options:");
    println!("  --list              List available audio devices and exit");
    println!("  --input <idx>       Input device index (default: system default)");
    println!("  --output <idx>      Output device index (default: system default)");
    println!("  --sample-rate <hz>  Sample rate (default: 48000)");
    println!("  --channels <n>      Number of channels (default: 1)");
    println!("  --format <fmt>      Sample format: i16 or f32 (default: i16)");
    println!("  --moq <path>        Use MoQ relay transport (e.g. anon/my-audio)");
    println!("  --identity <path>   Path to save/load iroh identity key");
    #[cfg(feature = "audio-macos")]
    println!("  --no-vpio           Disable Voice Processing IO (use cpal backend)");
}

fn list_devices() {
    println!("Input devices (microphones):");
    match AudioDevice::list_inputs() {
        Ok(devices) if !devices.is_empty() => {
            for d in &devices {
                println!("  [{}] {}", d.index, d.name);
            }
        }
        _ => println!("  (none found)"),
    }

    println!("\nOutput devices (speakers):");
    match AudioDevice::list_outputs() {
        Ok(devices) if !devices.is_empty() => {
            for d in &devices {
                println!("  [{}] {}", d.index, d.name);
            }
        }
        _ => println!("  (none found)"),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("xoq=info".parse()?)
                .add_directive("info".parse()?),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--list") {
        list_devices();
        return Ok(());
    }

    // Use persistent identity by default (stable endpoint ID across restarts)
    let mut builder = AudioServerBuilder::new().iroh_with_identity(".xoq_audio_server_key");
    let mut i = 1;

    while i < args.len() {
        match args[i].as_str() {
            "--input" if i + 1 < args.len() => {
                builder = builder.input_device(args[i + 1].parse()?);
                i += 2;
            }
            "--output" if i + 1 < args.len() => {
                builder = builder.output_device(args[i + 1].parse()?);
                i += 2;
            }
            "--sample-rate" if i + 1 < args.len() => {
                builder = builder.sample_rate(args[i + 1].parse()?);
                i += 2;
            }
            "--channels" if i + 1 < args.len() => {
                builder = builder.channels(args[i + 1].parse()?);
                i += 2;
            }
            "--format" if i + 1 < args.len() => {
                let fmt = match args[i + 1].as_str() {
                    "i16" => SampleFormat::I16,
                    "f32" => SampleFormat::F32,
                    other => anyhow::bail!("Unknown format: {} (use i16 or f32)", other),
                };
                builder = builder.sample_format(fmt);
                i += 2;
            }
            "--moq" if i + 1 < args.len() => {
                builder = builder.moq(&args[i + 1]);
                i += 2;
            }
            "--identity" if i + 1 < args.len() => {
                builder = builder.iroh_with_identity(&args[i + 1]);
                i += 2;
            }
            #[cfg(feature = "audio-macos")]
            "--no-vpio" => {
                builder = builder.use_vpio(false);
                i += 1;
            }
            "--help" | "-h" => {
                print_usage();
                return Ok(());
            }
            _ => {
                i += 1;
            }
        }
    }

    #[cfg(feature = "audio-macos")]
    let vpio_enabled = !args.iter().any(|a| a == "--no-vpio");
    #[cfg(not(feature = "audio-macos"))]
    let vpio_enabled = false;

    let mut server = builder.build().await?;

    println!("\n========================================");
    println!("Audio Server{}", if vpio_enabled { " (VPIO)" } else { "" });
    println!("ID: {}", server.id());
    println!(
        "Config: {}Hz, {}ch, {:?}",
        server.config().sample_rate,
        server.config().channels,
        server.config().sample_format
    );
    if vpio_enabled {
        println!("Backend: Voice Processing IO (AEC/NS/AGC)");
    }
    println!("========================================\n");

    tokio::select! {
        result = server.run() => {
            if let Err(e) = result {
                tracing::error!("Audio server error: {}", e);
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Shutting down...");
        }
    }

    Ok(())
}
