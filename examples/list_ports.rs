//! List available serial ports

use anyhow::Result;
use xoq::list_ports;

fn main() -> Result<()> {
    println!("Available serial ports:\n");

    let ports = list_ports()?;

    if ports.is_empty() {
        println!("  No serial ports found");
        return Ok(());
    }

    for port in ports {
        println!("  {}", port.name);
        match port.port_type {
            xoq::PortType::Usb {
                vid,
                pid,
                manufacturer,
                product,
            } => {
                println!("    Type: USB");
                println!("    VID:PID: {:04x}:{:04x}", vid, pid);
                if let Some(m) = manufacturer {
                    println!("    Manufacturer: {}", m);
                }
                if let Some(p) = product {
                    println!("    Product: {}", p);
                }
            }
            xoq::PortType::Pci => println!("    Type: PCI"),
            xoq::PortType::Bluetooth => println!("    Type: Bluetooth"),
            xoq::PortType::Unknown => println!("    Type: Unknown"),
        }
        println!();
    }

    Ok(())
}
