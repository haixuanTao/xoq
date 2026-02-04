# XoQ — Anything over QUIC

Remote hardware access for robotics over the internet.

- **100% AI-native** — entire codebase generated with AI for low level optimization
- **100% Rust** — single binary, no runtime deps, no GC
- **100% hardware-optimized** — zero-copy paths, no intermediary abstractions, direct V4L2/AVFoundation/SocketCAN access
- **100% cross-compatible** — virtualize your robot and connect from anywhere, any OS (Linux, macOS, Windows), any language (Python, Rust, JavaScript)

```
Robot Hardware          Network              Clients
┌──────────┐     ┌──────────────────┐     ┌──────────────┐
│ Camera   │     │  Iroh (P2P)      │     │ Python       │
│ Serial   │────▶│  MoQ  (Relay)    │────▶│ JavaScript   │
│ CAN Bus  │     │  QUIC / TLS 1.3  │     │ Rust         │
└──────────┘     └──────────────────┘     └──────────────┘
  XoQ Server                                XoQ Client
```

## Comparison Table

|                         | XoQ                                 | WebRTC                                      | rosbridge       | gRPC               | ROS 2 (DDS)         |
| ----------------------- | ----------------------------------- | ------------------------------------------- | --------------- | ------------------ | ------------------- |
| **Setup**               | Single binary, Public Key + Relay   | Complex (Signaling + STUN/TURN + SDP + ICE) | ROS + rosbridge | Protobuf toolchain | ROS + DDS vendor    |
| **Encryption**          | Always on (TLS 1.3)                 | Always on (DTLS)                            | None by default | Optional TLS       | Optional SROS2      |
| **P2P / NAT traversal** | Built-in (Iroh)                     | ICE/STUN/TURN                               | No              | No                 | No                  |
| **Zero-copy HW**        | Yes (V4L2, AVFoundation, SocketCAN) | No                                          | No              | No                 | No                  |
| **Browser**             | WebTransport (MoQ)                  | Yes                                         | Yes             | grpc-web (limited) | No                  |
| **Transport**           | QUIC / TLS 1.3                      | DTLS / SRTP                                 | TCP / WebSocket | HTTP/2             | DDS (UDP multicast) |
| **Languages**           | Python, JS, Rust                    | JS, native SDKs                             | Any (JSON)      | Any (codegen)      | Python, C++         |
| **Cross-platform**      | Linux, macOS, Windows               | All (browser)                               | Linux (primary) | All                | Linux (primary)     |

## Use Cases

| Use Case                              | How XoQ Helps                                                                                                                                              |
| ------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Remote teleoperation**              | Full-duplex serial/CAN bridging with sub-ms overhead — operate arms, grippers, and mobile bases over the internet as if wired locally                      |
| **RL / VLM online training at scale** | Stream camera + proprioception from a fleet of robots to GPU clusters; drop-in Python clients mean training scripts don't change                           |
| **Inference at scale**                | Send model outputs back to robots over the same QUIC connection; encrypted P2P means no VPN or port forwarding per robot                                   |
| **CI/CD of robots**                   | Run hardware-in-the-loop tests from anywhere — flash firmware over remote serial, validate CAN protocols, and capture camera feeds without physical access |

---

## Roadmap

| Feature                          | Description                                                                                                                               |
| -------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------- |
| Audio                            | OS-level hardware AEC, noise cancellation, and compression                                                                                |
| CMAF Format                      | Fragmented MP4 for dataset recording with robotic data track for Training dataset streaming (online + offline, compatible with inference) |
| Framework compat                 | dora-rs, ROS, ROS2, and LeRobot                                                                                                           |
| Embedded targets                 | ESP32 / STM32 (server and client)                                                                                                         |
| Intel RealSense / lidar / Orbbec | `pyrealsense2` drop-in replacement                                                                                                        |
| C/C++ bindings                   | Via Rust ABI                                                                                                                              |

---

## Reference

### Hardware Support

| Device  | Server Platform       | HW Encoding                | Feature Flags           |
| ------- | --------------------- | -------------------------- | ----------------------- |
| Camera  | Linux (V4L2)          | NVIDIA NVENC (H.264/HEVC)  | `camera`, `nvenc`       |
| Camera  | macOS (AVFoundation)  | Apple VideoToolbox (H.264) | `camera-macos`, `vtenc` |
| Serial  | Linux, macOS, Windows | —                          | `serial`                |
| CAN Bus | Linux (SocketCAN)     | —                          | `can`                   |

## Getting Started

### Bridge a camera

Server (on the machine with the camera):

```bash
cargo run --example camera_server --features "iroh,camera-macos,vtenc" -- 0 --h264
# Linux Nvidia: --features "iroh,camera,nvenc"
```

Python client:

```python
import xoq_cv2

cap = xoq_cv2.VideoCapture("<server-endpoint-id>")
while cap.isOpened():
    ret, frame = cap.read()  # numpy array (H, W, 3)
    if not ret:
        break
    print(f"Frame: {frame.shape}")
cap.release()
```

### Bridge a serial port

Server:

```bash
cargo run --example serial_server --features "iroh,serial" -- /dev/ttyUSB0 1000000
```

Python client:

```python
import serial

port = serial.Serial("<server-endpoint-id>", baudrate=1000000)
port.write(b"\xff\xff\x01\x04\x02\x00\x00\xf8")
response = port.read(64)
print(response)
port.close()
```

### Bridge a CAN bus

Server:

```bash
cargo run --example can_server --features "iroh,can" -- can0:fd
```

Python client:

```python
import can

bus = can.Bus("<server-endpoint-id>")
bus.send(can.Message(arbitration_id=0x123, data=[0x01, 0x02, 0x03]))
for msg in bus:
    print(f"ID={msg.arbitration_id:#x} data={msg.data.hex()}")
```

---

### Client Libraries

#### Python

Drop-in replacements for popular hardware libraries — same API, remote hardware:

| Package      | Import           | Replaces      | API Surface                                                    |
| ------------ | ---------------- | ------------- | -------------------------------------------------------------- |
| `xoq-serial` | `import serial`  | pyserial      | `Serial`: read, write, readline, context manager               |
| `xoq-can`    | `import can`     | python-can    | `Bus`, `Message` with full CAN FD fields, send/recv/iterator   |
| `xoq-opencv` | `import xoq_cv2` | opencv-python | `VideoCapture`: read, isOpened, release — returns numpy arrays |

Install individually or all at once:

```bash
pip install xoq-serial xoq-can xoq-opencv
# or
pip install xoq[all]
```

Build from source with maturin:

```bash
cd packages/serial && maturin develop --release
cd packages/can && maturin develop --release
cd packages/cv2 && maturin develop --features videotoolbox --release
```

#### JavaScript / TypeScript

```bash
npm install xoq
```

MoQ pub/sub for web — serial streaming and camera frames over WebTransport.

#### Rust

```toml
[dependencies]
xoq = { version = "0.3", features = ["serial-remote", "camera-remote", "can-remote"] }
```

Feature flags for remote access: `serial-remote`, `camera-remote`, `can-remote`.

Clients target macOS, Linux, and Windows. Future: C/C++ bindings via Rust ABI.

### Examples

| Example            | Description                                                | Required Features                |
| ------------------ | ---------------------------------------------------------- | -------------------------------- |
| `camera_server`    | Streams local cameras to remote clients (JPEG or H.264)    | `iroh` + `camera`/`camera-macos` |
| `camera_client`    | Receives and displays frames from remote camera            | `iroh`, `camera`                 |
| `camera_viewer.py` | Python OpenCV viewer for camera streams                    | (Python)                         |
| `serial_server`    | Bridges a local serial port for remote access              | `iroh`, `serial`                 |
| `serial_client`    | Connects to a remote serial port                           | `iroh`, `serial`                 |
| `can_server`       | Bridges local CAN interfaces for remote access             | `iroh`, `can`                    |
| `can_client`       | Connects to a remote CAN interface                         | `iroh`, `can`                    |
| `rustypot_remote`  | Drives STS3215 servos over a remote serial port (rustypot) | `iroh`, `serial`                 |
| `so100_teleop`     | Teleoperate a remote SO-100 arm from a local leader arm    | `iroh`, `serial`                 |
| `reachy_mini`      | Reachy Mini robot control over remote serial               | `iroh`, `serial`                 |
| `moq_test`         | MoQ relay publish/subscribe diagnostic test                | —                                |

### ALPN Protocols

| Protocol            | Purpose                                   |
| ------------------- | ----------------------------------------- |
| `xoq/p2p/0`         | Generic P2P communication (serial, CAN)   |
| `xoq/camera/0`      | Camera streaming (legacy, JPEG)           |
| `xoq/camera-jpeg/0` | Camera streaming with JPEG frames         |
| `xoq/camera-h264/0` | Camera streaming with H.264 encoding      |
| `xoq/camera-hevc/0` | Camera streaming with HEVC/H.265 encoding |
| `xoq/camera-av1/0`  | Camera streaming with AV1 encoding        |

### Cargo Feature Flags

| Flag            | Purpose                                          |
| --------------- | ------------------------------------------------ |
| `iroh`          | Iroh P2P transport                               |
| `serial`        | Local serial port access (server)                |
| `camera`        | V4L2 camera capture (Linux server)               |
| `camera-macos`  | AVFoundation camera capture (macOS server)       |
| `nvenc`         | NVIDIA NVENC H.264/HEVC encoding (Linux server)  |
| `vtenc`         | Apple VideoToolbox H.264 encoding (macOS server) |
| `can`           | SocketCAN access (Linux server)                  |
| `serial-remote` | Remote serial client (cross-platform)            |
| `camera-remote` | Remote camera client (cross-platform)            |
| `can-remote`    | Remote CAN client (cross-platform)               |
| `image`         | Image processing support                         |

### License

Apache-2.0

---

## Explanation

> How and why XOQ works the way it does

### Architecture

#### Transport Layer

```
                 ┌─────────┐     ┌─────────┐
                 │  Iroh   │     │   MoQ   │
                 │  (P2P)  │     │ (Relay) │
                 └────┬────┘     └────┬────┘
                      └───────┬───────┘
                           QUIC / TLS 1.3
                              │
                    ┌─────────┴─────────┐
                    │  Application      │
                    │  Protocol (ALPN)  │
                    └───────────────────┘
```

Both transports converge on the same QUIC layer. The ALPN protocol determines what flows over the connection.

#### Zero-Copy Data Path (macOS Camera)

```
AVFoundation callback
       │
       ▼  (0 copies — CFRetain on CVPixelBuffer)
RetainedPixelBuffer
       │
       ▼  (0 copies — pointer pass to VideoToolbox)
VtEncoder.encode_pixel_buffer()
       │
       ▼  H.264 NAL units
CMAF muxer  (pre-allocated Vec)
       │
       ▼  fragmented MP4 segments
QUIC send
```

From camera capture to network send: **zero memcpy of pixel data**. The CVPixelBuffer pointer is retained (CFRetain) and passed directly to VideoToolbox for hardware encoding. The encoder outputs compressed NAL units which are muxed into CMAF fragments and sent over QUIC.

#### Thread Model

```
Camera Server:
├─ Capture thread       (AVFoundation/V4L2, blocking I/O)
├─ Encoder task         (async, hardware-accelerated — NVENC or VideoToolbox)
├─ Muxer task           (async, CMAF fragmentation)
└─ Transport task       (async, QUIC streams)

CAN / Serial Server:
├─ Reader thread        (blocking I/O) → channel(16)
├─ Writer thread        (blocking I/O) ← channel(1)
└─ Connection handler   (async, batching)
```

Camera uses dedicated threads for capture (hardware I/O is blocking) with async tasks for encoding, muxing, and transport. CAN and serial servers use a reader/writer thread pair with bounded channels bridging to the async connection handler.

#### ALPN Negotiation

Clients negotiate the best encoding by trying ALPNs in preference order (H.264 > JPEG > legacy).
