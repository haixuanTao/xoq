//! Example showing serialport-compatible API for remote serial ports.
//!
//! This example demonstrates how xoq::serialport can be used as a drop-in
//! replacement for the `serialport` crate.
//!
//! Usage: serial_port_compat <server-endpoint-id>

use anyhow::Result;
use std::env;
use std::io::{BufRead, BufReader, Write};
use std::time::Duration;

// Drop-in replacement: just change `use serialport` to `use xoq::serialport`
use xoq::serialport;

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        println!("Usage: serial_port_compat <server-endpoint-id>");
        println!("\nThis example demonstrates serialport-compatible API:");
        println!("  - serialport::new(id).open() - same as serialport crate");
        println!("  - Implements std::io::Read and std::io::Write");
        println!("  - Works with BufReader for line-based I/O");
        return Ok(());
    }

    let server_id = &args[1];
    println!("Connecting to: {}", server_id);

    // ========================================
    // Drop-in serialport replacement
    // ========================================

    // Original serialport crate: serialport::new("/dev/ttyUSB0", 115200).open()?
    // Remote xoq version:    serialport::new(server_id).open()?
    let mut port = serialport::new(server_id)
        .timeout(Duration::from_secs(1))
        .open()?;

    println!("Connected!");
    println!("Port name: {:?}", port.name());
    println!("Timeout: {:?}", port.timeout());

    // Set a shorter timeout
    port.set_timeout(Duration::from_millis(500))?;
    println!("New timeout: {:?}", port.timeout());

    // Write using std::io::Write trait
    port.write_all(b"AT\r\n")?;
    println!("Sent: AT");

    // Read using std::io::Read trait via BufReader
    let mut reader = BufReader::new(&mut port);
    let mut response = String::new();

    match reader.read_line(&mut response) {
        Ok(_) => println!("Response: {}", response.trim()),
        Err(e) => println!("Read error (timeout?): {}", e),
    }

    // Check buffer status
    println!("Bytes in buffer: {}", port.bytes_to_read()?);

    // Clear buffers
    port.clear_input()?;
    println!("Buffer cleared");

    // Direct methods also available
    port.write_bytes(b"ATI\r\n")?;
    println!("Sent: ATI");

    // Read line directly
    match port.read_line() {
        Ok(line) => println!("Response: {}", line.trim()),
        Err(e) => println!("Read error: {}", e),
    }

    println!("\nDone!");
    Ok(())
}
