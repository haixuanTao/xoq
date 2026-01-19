#!/usr/bin/env python3
"""Serial port bridge client - connects to remote serial port.

Usage: serial_client_iroh.py <server-endpoint-id>
"""

import sys
import threading

import xoq


def main():
    if len(sys.argv) < 2:
        print("Usage: serial_client_iroh.py <server-endpoint-id>")
        return

    server_id = sys.argv[1]
    print(f"Connecting to server: {server_id}")

    # Connect to the remote serial port
    client = xoq.Client(server_id)
    print("Connected!")
    print("Type to send, Ctrl+C to exit.\n")

    # Flag to stop threads
    running = True

    # Thread to read from remote serial port and print to stdout
    def read_loop():
        while running:
            try:
                data = client.read(1024)
                if data:
                    # Print received data (decode if text, or show hex)
                    try:
                        print(data.decode("utf-8"), end="", flush=True)
                    except UnicodeDecodeError:
                        print(f"[hex: {data.hex()}]", flush=True)
            except Exception as e:
                if running:
                    print(f"\nRead error: {e}")
                break

    # Start reader thread
    reader = threading.Thread(target=read_loop, daemon=True)
    reader.start()

    # Main thread: read from stdin and send to remote serial port
    try:
        while True:
            line = input()
            client.write_str(line + "\n")
    except KeyboardInterrupt:
        print("\nDisconnecting...")
    except EOFError:
        pass
    finally:
        running = False


if __name__ == "__main__":
    main()
