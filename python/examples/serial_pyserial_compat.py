#!/usr/bin/env python3
"""Example showing pyserial-compatible API for remote serial ports.

This example demonstrates how xoq.Serial can be used as a drop-in
replacement for serial.Serial, connecting to a remote serial port
over iroh P2P instead of a local port.

Usage: serial_pyserial_compat.py <server-endpoint-id>
"""

import sys

import xoq


def main():
    if len(sys.argv) < 2:
        print("Usage: serial_pyserial_compat.py <server-endpoint-id>")
        print("\nThis script demonstrates pyserial-compatible API:")
        print("  - xoq.Serial(port) instead of serial.Serial(port)")
        print("  - Same methods: read(), write(), readline(), etc.")
        return

    server_id = sys.argv[1]

    # ========================================
    # pyserial-compatible usage
    # ========================================

    # Instead of: ser = serial.Serial('/dev/ttyUSB0', 115200)
    # Use:        ser = xoq.Serial(server_id)

    # Context manager works just like pyserial
    with xoq.Serial(server_id) as ser:
        print(f"Connected to: {ser.port}")
        print(f"is_open={ser.is_open}")

        # Write bytes (returns number of bytes written)
        n = ser.write(b"AT\r\n")
        print(f"Wrote {n} bytes")

        # Read a line (blocks until newline received)
        response = ser.readline()
        print(f"Response: {response}")

        # Read until custom terminator (e.g., "OK\r\n")
        ser.write(b"ATI\r\n")
        response = ser.read_until(b"\r\n")
        print(f"Read until CRLF: {response}")

        # Read specific number of bytes
        data = ser.read(10)
        print(f"Read {len(data)} bytes: {data}")

        # Check bytes waiting in buffer
        print(f"Bytes in buffer: {ser.in_waiting}")

        # Clear receive buffer
        ser.reset_input_buffer()
        print(f"Buffer cleared, now: {ser.in_waiting}")

        # Flush (no-op for network, but API-compatible)
        ser.flush()

    # After context manager exits, port is closed
    print("Connection closed")


def interactive_example():
    """Interactive mode similar to a serial terminal."""
    if len(sys.argv) < 2:
        return

    server_id = sys.argv[1]

    with xoq.Serial(server_id) as ser:
        print("Interactive mode. Type commands, Ctrl+C to exit.\n")

        import threading

        # Reader thread
        def reader():
            while ser.is_open:
                try:
                    line = ser.readline()
                    if line:
                        print(f"< {line.decode('utf-8', errors='replace')}", end="")
                except Exception:
                    break

        t = threading.Thread(target=reader, daemon=True)
        t.start()

        # Writer loop
        try:
            while True:
                cmd = input("> ")
                ser.write((cmd + "\r\n").encode())
        except (KeyboardInterrupt, EOFError):
            print("\nExiting...")


if __name__ == "__main__":
    main()
