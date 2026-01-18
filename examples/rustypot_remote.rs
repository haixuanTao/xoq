//! Example showing rustypot STS3215 controller with xoq remote serial port.
//!
//! This example continuously reads present position from servos over a remote
//! serial port connection via iroh P2P, using the rustypot crate.
//!
//! Setup:
//! 1. Run a serial bridge server on the machine with the actual servos:
//!    `cargo run --example serial_server --features "iroh,serial" -- /dev/ttyUSB0 1000000`
//!
//! 2. Copy the server endpoint ID and run this client:
//!    `cargo run --example rustypot_remote --features "iroh,serial" -- <server-endpoint-id>`

use anyhow::Result;
use rustypot::servo::feetech::sts3215::Sts3215Controller;
use std::env;
use std::thread;
use std::time::Duration;

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        println!("Usage: rustypot_remote <server-endpoint-id>");
        println!("\nThis example connects to a remote serial port over iroh P2P");
        println!("and continuously reads STS3215 servo positions using rustypot.");
        println!("\nFirst, start a serial server on the machine with the servos:");
        println!("  cargo run --example serial_server --features \"iroh,serial\" -- /dev/ttyUSB0 1000000");
        return Ok(());
    }

    let server_id = &args[1];
    println!("Connecting to remote serial port: {}", server_id);

    // Open remote serial port via iroh P2P
    // RemoteSerialPort implements serialport::SerialPort directly
    let port = xoq::serialport::new(server_id)
        .timeout(Duration::from_millis(1000))
        .open()?;

    // Create rustypot controller
    let mut controller = Sts3215Controller::new()
        .with_protocol_v1()
        .with_serial_port(Box::new(port));

    println!("Connected to remote serial port!");
    println!("Reading servo positions continuously (Ctrl+C to stop)...\n");

    // Continuous read loop using rustypot
    loop {
        match controller.sync_read_present_position(&[1, 2]) {
            Ok(positions) => {
                println!("Positions: {:?}", positions);
            }
            Err(e) => {
                println!("Read error: {}", e);
            }
        }

        thread::sleep(Duration::from_millis(50));
    }
}
