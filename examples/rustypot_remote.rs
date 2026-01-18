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
use std::io::{Read, Write};
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

    // Open remote serial port via iroh P2P and wrap for rustypot compatibility
    let remote_port = xoq::serialport::new(server_id)
        .timeout(Duration::from_millis(1000))
        .open()?;
    let port = SerialPortWrapper::new(remote_port);

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

/// Wrapper to implement serialport::SerialPort for xoq::RemoteSerialPort
struct SerialPortWrapper {
    inner: xoq::serialport::RemoteSerialPort,
    timeout: Duration,
}

impl SerialPortWrapper {
    fn new(port: xoq::serialport::RemoteSerialPort) -> Self {
        let timeout = port.timeout();
        Self { inner: port, timeout }
    }
}

impl Read for SerialPortWrapper {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Write for SerialPortWrapper {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl serialport::SerialPort for SerialPortWrapper {
    fn name(&self) -> Option<String> {
        self.inner.name()
    }

    fn baud_rate(&self) -> serialport::Result<u32> {
        Ok(1_000_000)
    }

    fn data_bits(&self) -> serialport::Result<serialport::DataBits> {
        Ok(serialport::DataBits::Eight)
    }

    fn flow_control(&self) -> serialport::Result<serialport::FlowControl> {
        Ok(serialport::FlowControl::None)
    }

    fn parity(&self) -> serialport::Result<serialport::Parity> {
        Ok(serialport::Parity::None)
    }

    fn stop_bits(&self) -> serialport::Result<serialport::StopBits> {
        Ok(serialport::StopBits::One)
    }

    fn timeout(&self) -> Duration {
        self.timeout
    }

    fn set_baud_rate(&mut self, _: u32) -> serialport::Result<()> {
        Ok(())
    }

    fn set_data_bits(&mut self, _: serialport::DataBits) -> serialport::Result<()> {
        Ok(())
    }

    fn set_flow_control(&mut self, _: serialport::FlowControl) -> serialport::Result<()> {
        Ok(())
    }

    fn set_parity(&mut self, _: serialport::Parity) -> serialport::Result<()> {
        Ok(())
    }

    fn set_stop_bits(&mut self, _: serialport::StopBits) -> serialport::Result<()> {
        Ok(())
    }

    fn set_timeout(&mut self, timeout: Duration) -> serialport::Result<()> {
        self.timeout = timeout;
        let _ = self.inner.set_timeout(timeout);
        Ok(())
    }

    fn write_request_to_send(&mut self, _: bool) -> serialport::Result<()> {
        Ok(())
    }

    fn write_data_terminal_ready(&mut self, _: bool) -> serialport::Result<()> {
        Ok(())
    }

    fn read_clear_to_send(&mut self) -> serialport::Result<bool> {
        Ok(true)
    }

    fn read_data_set_ready(&mut self) -> serialport::Result<bool> {
        Ok(true)
    }

    fn read_ring_indicator(&mut self) -> serialport::Result<bool> {
        Ok(false)
    }

    fn read_carrier_detect(&mut self) -> serialport::Result<bool> {
        Ok(true)
    }

    fn bytes_to_read(&self) -> serialport::Result<u32> {
        self.inner.bytes_to_read().map_err(|e| {
            serialport::Error::new(serialport::ErrorKind::Io(std::io::ErrorKind::Other), e.to_string())
        })
    }

    fn bytes_to_write(&self) -> serialport::Result<u32> {
        Ok(0)
    }

    fn clear(&self, _: serialport::ClearBuffer) -> serialport::Result<()> {
        Ok(())
    }

    fn try_clone(&self) -> serialport::Result<Box<dyn serialport::SerialPort>> {
        Err(serialport::Error::new(
            serialport::ErrorKind::Io(std::io::ErrorKind::Unsupported),
            "Clone not supported",
        ))
    }

    fn set_break(&self) -> serialport::Result<()> {
        Ok(())
    }

    fn clear_break(&self) -> serialport::Result<()> {
        Ok(())
    }
}
