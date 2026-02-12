# CLAUDE.md - Project Context for AI Assistants

## Project: XoQ (X-Embodiment over QUIC)

P2P and relay communication for robotics. Uses iroh (QUIC) for peer-to-peer connections and MoQ for relay-based pub/sub.

## Resolved: Latency Spikes Were WiFi, Not QUIC

**GitHub issue:** https://github.com/n0-computer/iroh/issues/3915

### Root Cause: WiFi Latency

The ~90ms latency spikes were caused by **WiFi**, not QUIC coalescing. Raw ICMP ping over WiFi showed identical spikes (min 3ms, max 94ms). WiFi has inherent latency variance due to CSMA/CA contention and beacon intervals.

### Solution: Use Ethernet

With Ethernet, latency is consistent:
- Min: 1.4ms, Max: 10.5ms, Avg: 2.0ms, P99: 5.2ms
- **Zero spikes >50ms**

### Important: Don't Use Datagrams Mode

`use_datagrams(true)` causes connection issues where the server waits indefinitely for stream. **Use streams mode** (`use_datagrams(false)`) for reliable operation.

### Files

- `src/iroh.rs` — Transport config, NoopController, connection builders
- `src/serialport_impl.rs` — `RemoteSerialPort` blocking API (client side)
- `src/serial_server.rs` — Serial bridge server
- `examples/so100_teleop.rs` — Teleop example (uses streams mode)
- `examples/iroh_latency_test.rs` — Latency/jitter measurement tool

## MoQ Relay

### cdn.moq.dev Limitation

cdn.moq.dev does NOT forward announcements between separate WebTransport sessions. Cross-session pub/sub is impossible through this relay.

### Solution: Self-hosted moq-relay

Run your own relay from https://github.com/kixelated/moq-rs:

```bash
# Build
cargo build --release -p moq-relay

# Run (with self-signed TLS)
moq-relay --server-bind 0.0.0.0:4443 --tls-generate localhost --auth-public anon
```

### Usage

```rust
// Publisher
let (_pub, mut track) = MoqBuilder::new()
    .relay("https://your-relay:4443")
    .path("anon/my-channel")
    .disable_tls_verify()  // for self-signed certs
    .connect_publisher_with_track("video")
    .await?;
track.write_str("hello");

// Subscriber (separate process)
let mut sub = MoqBuilder::new()
    .relay("https://your-relay:4443")
    .path("anon/my-channel")
    .disable_tls_verify()
    .connect_subscriber()
    .await?;
let mut reader = sub.subscribe_track("video").await?.unwrap();
let data = reader.read_string().await?;
```

### Files

- `src/moq.rs` — MoqBuilder, MoqPublisher, MoqSubscriber, MoqStream
- `examples/moq_test.rs` — Simple pub/sub test (`pub`, `sub` modes)

## CAN Bus Setup

### DANGER: NEVER Send CAN Commands Without Explicit User Approval

**CAN bus controls physical motors and actuators. Sending arbitrary CAN frames (cansend, write commands, enable/disable) can cause unexpected and dangerous motor movements.** NEVER send CAN commands to diagnose issues — only read/monitor (candump, ip link show). Always ask the user before sending ANY data on the CAN bus.

### Hardware

PCAN USB Pro FD adapters (4 channels: can0–can3). Connected via USB to the robot PC (172.18.133.111).

### Interface Setup (all 4 channels)

```bash
# 1. Stop the can-server first
systemctl --user stop can-server

# 2. Bring all interfaces down
sudo ip link set can0 down
sudo ip link set can1 down
sudo ip link set can2 down
sudo ip link set can3 down

# 3. Bring them back up: CAN FD (1 Mbps nominal, 5 Mbps data) + auto-restart from BUS-OFF
sudo ip link set can0 up type can bitrate 1000000 dbitrate 5000000 fd on restart-ms 100
sudo ip link set can1 up type can bitrate 1000000 dbitrate 5000000 fd on restart-ms 100
sudo ip link set can2 up type can bitrate 1000000 dbitrate 5000000 fd on restart-ms 100
sudo ip link set can3 up type can bitrate 1000000 dbitrate 5000000 fd on restart-ms 100

# 4. Restart the can-server (gets fresh socket handles)
systemctl --user start can-server
```

**Important:** The can-server must be stopped before reconfiguring interfaces, otherwise it holds stale socket handles and all CAN writes fail with "No such device or address (os error 6)".

### CAN Interfaces Go DOWN (BUS-OFF Recovery)

By default `restart-ms` is 0, meaning **no automatic recovery** from BUS-OFF state. When CAN frames are sent but no device ACKs them (motors off, cable disconnected), the TX error counter climbs rapidly until the controller enters BUS-OFF and the interface goes DOWN permanently.

**Fix:** Always set `restart-ms 100` in the `ip link set up` command (see above) so interfaces auto-recover after 100ms.

### CAN Server

```bash
# Run the MoQ CAN server (publishes motor state, receives commands)
can-server can0:fd can1:fd --moq-relay https://cdn.1ms.ai
```

Publishes per-interface at `anon/xoq-can-can0/state` and `anon/xoq-can-can1/state`. Subscribes to `anon/xoq-can-can0/commands` and `anon/xoq-can-can1/commands` for motor commands.

### Damiao Motor Protocol

- **Enable MIT mode:** CAN data `[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFC]`
- **Disable MIT mode:** CAN data `[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFD]`
- **Query (zero-torque):** MIT command with p=0, v=0, kp=0, kd=0, t=0 — returns motor state without applying torque
- Motors must be in MIT mode before they respond to zero-torque queries

### Files

- `src/bin/can_server.rs` — CAN server binary (MoQ pub/sub + SocketCAN)
- `src/moq_can_server.rs` — MoQ CAN bridge logic
- `src/socketcan_impl.rs` — SocketCAN interface
