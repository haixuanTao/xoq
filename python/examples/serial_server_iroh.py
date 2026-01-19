#!/usr/bin/env python3
"""Serial port bridge server - all forwarding handled in Rust.

Usage: serial_server_iroh.py <port> [baud_rate]
Example: serial_server_iroh.py /dev/ttyUSB0 115200
"""

import sys

import xoq


def main():
    if len(sys.argv) < 2:
        print("Usage: serial_server_iroh.py <port> [baud_rate]")
        print("Example: serial_server_iroh.py /dev/ttyUSB0 115200")
        print("\nAvailable ports:")
        for port in xoq.list_ports():
            print(f"  {port.name} - {port.port_type}")
        return

    port_name = sys.argv[1]
    baud_rate = int(sys.argv[2]) if len(sys.argv) > 2 else 115200

    # Create server - opens serial port and starts iroh
    server = xoq.Server(port_name, baud_rate, identity_path=".xoq_serial_bridge_key")

    print("Server started")
    print(f"Port: {port_name} @ {baud_rate} baud")
    print(f"Server ID: {server.id()}")
    print("Waiting for connections...")

    # Run forever - all forwarding handled in Rust
    server.run()


if __name__ == "__main__":
    main()
