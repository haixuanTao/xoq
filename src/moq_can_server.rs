//! MoQ CAN bridge server - bridges local CAN interface to remote clients via MoQ relay.
//!
//! Architecture: Reuses the same 2 persistent OS threads (reader + writer) as the iroh
//! CAN server, but bridges to MoQ tracks instead of QUIC streams.

use anyhow::Result;
use socketcan::Socket;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Read the CAN bus error state by parsing `ip -details link show`.
/// Returns e.g. "ERROR-ACTIVE", "ERROR-PASSIVE", "BUS-OFF", or None if unknown.
fn can_bus_state(interface: &str) -> Option<String> {
    // Try sysfs first (works on some adapters)
    if let Ok(s) = std::fs::read_to_string(format!("/sys/class/net/{}/can_state", interface)) {
        return Some(s.trim().to_uppercase());
    }
    // Fall back to parsing `ip -details link show` output
    let output = std::process::Command::new("ip")
        .args(["-details", "link", "show", interface])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    // Look for "state ERROR-ACTIVE", "state ERROR-PASSIVE", "state BUS-OFF"
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("can ") {
            // e.g. "can <FD> state ERROR-ACTIVE (berr-counter tx 0 rx 0)"
            if let Some(pos) = rest.find("state ") {
                let after = &rest[pos + 6..];
                let state = after.split_whitespace().next()?;
                return Some(state.to_string());
            }
        }
    }
    None
}

use crate::can::{CanFdFrame, CanFrame};
use crate::can_types::{wire, AnyCanFrame};
use crate::moq::MoqBuilder;

/// A server that bridges a local CAN interface to remote clients over MoQ relay.
pub struct MoqCanServer {
    can_write_tx: tokio::sync::mpsc::Sender<AnyCanFrame>,
    can_read_rx: Option<tokio::sync::mpsc::Receiver<AnyCanFrame>>,
    interface: String,
    enable_fd: bool,
}

impl MoqCanServer {
    /// Create a new MoQ CAN bridge server.
    pub fn new(interface: &str, enable_fd: bool) -> Result<Self> {
        let (can_read_tx, can_read_rx) = tokio::sync::mpsc::channel::<AnyCanFrame>(128);
        let (can_write_tx, can_write_rx) = tokio::sync::mpsc::channel::<AnyCanFrame>(1);
        let write_count = Arc::new(AtomicU64::new(0));

        // Check CAN bus state on startup — bail if not healthy
        match can_bus_state(interface) {
            Some(ref state) if state == "ERROR-ACTIVE" => {
                tracing::info!("[{}] CAN bus state: {}", interface, state);
            }
            Some(ref state) => {
                anyhow::bail!(
                    "[{}] CAN bus is {} — motors may be disconnected or unpowered. \
                     Fix: power on motors, then: sudo ip link set {} down && sudo ip link set {} up type can bitrate 1000000 fd on dbitrate 5000000",
                    interface, state, interface, interface,
                );
            }
            None => tracing::warn!(
                "[{}] Could not read CAN bus state (will continue anyway)",
                interface
            ),
        }

        if enable_fd {
            let socket = socketcan::CanFdSocket::open(interface).map_err(|e| {
                anyhow::anyhow!("Failed to open CAN FD socket on {}: {}", interface, e)
            })?;
            socket
                .set_read_timeout(Duration::from_millis(10))
                .map_err(|e| anyhow::anyhow!("Failed to set read timeout: {}", e))?;
            tracing::info!("CAN FD socket opened on {}", interface);
            let socket = Arc::new(socket);

            let socket_reader = Arc::clone(&socket);
            let wc_reader = Arc::clone(&write_count);
            std::thread::Builder::new()
                .name(format!("can-read-{}", interface))
                .spawn(move || can_reader_thread_fd(socket_reader, can_read_tx, wc_reader))?;

            let socket_writer = Arc::clone(&socket);
            let wc_writer = Arc::clone(&write_count);
            std::thread::Builder::new()
                .name(format!("can-write-{}", interface))
                .spawn(move || can_writer_thread_fd(socket_writer, can_write_rx, wc_writer))?;
        } else {
            let socket = socketcan::CanSocket::open(interface).map_err(|e| {
                anyhow::anyhow!("Failed to open CAN socket on {}: {}", interface, e)
            })?;
            socket
                .set_read_timeout(Duration::from_millis(10))
                .map_err(|e| anyhow::anyhow!("Failed to set read timeout: {}", e))?;
            tracing::info!("CAN socket opened on {}", interface);
            let socket = Arc::new(socket);

            let socket_reader = Arc::clone(&socket);
            let wc_reader = Arc::clone(&write_count);
            std::thread::Builder::new()
                .name(format!("can-read-{}", interface))
                .spawn(move || can_reader_thread_std(socket_reader, can_read_tx, wc_reader))?;

            let socket_writer = Arc::clone(&socket);
            let wc_writer = Arc::clone(&write_count);
            std::thread::Builder::new()
                .name(format!("can-write-{}", interface))
                .spawn(move || can_writer_thread_std(socket_writer, can_write_rx, wc_writer))?;
        }

        Ok(Self {
            can_write_tx,
            can_read_rx: Some(can_read_rx),
            interface: interface.to_string(),
            enable_fd,
        })
    }

    /// Get the interface name.
    pub fn interface(&self) -> &str {
        &self.interface
    }

    /// Whether CAN FD is enabled.
    pub fn is_fd(&self) -> bool {
        self.enable_fd
    }

    /// Run the MoQ CAN bridge, connecting to the relay and bridging until disconnected.
    ///
    /// Uses broadcast pattern for 1-to-many state fan-out and many-to-1 command collection:
    /// - State: Published to `{path}/state` track "can" (all browsers receive CAN frames)
    /// - Commands: Subscribed from `{path}/commands` track "can" (any browser can send commands)
    ///
    /// Set `insecure` to true for self-hosted relays with self-signed TLS certificates.
    pub async fn run(&mut self, relay: &str, path: &str, insecure: bool) -> Result<()> {
        tracing::info!(
            "MoQ CAN server connecting to {}/{} for interface {}...",
            relay,
            path,
            self.interface
        );

        // State broadcast: CAN → all browsers
        let mut builder = MoqBuilder::new().relay(relay);
        if insecure {
            builder = builder.disable_tls_verify();
        }

        let (_publisher, mut state_writer) = builder
            .clone()
            .path(&format!("{}/state", path))
            .connect_publisher_with_track("can")
            .await?;
        tracing::info!("State broadcast connected on {}/state", path);

        let mut can_read_rx = self
            .can_read_rx
            .take()
            .expect("CAN read receiver should be available");

        // Drain stale frames
        let mut drained = 0;
        while can_read_rx.try_recv().is_ok() {
            drained += 1;
        }
        if drained > 0 {
            tracing::info!("Drained {} stale CAN frames from buffer", drained);
        }

        let can_write_tx = self.can_write_tx.clone();

        // Periodic CAN bus health monitor — exits process if bus degrades
        // (systemd Restart=always will restart us, and we'll bail on startup if still bad)
        let monitor_iface = self.interface.clone();
        let bus_monitor = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let state = tokio::task::spawn_blocking({
                    let iface = monitor_iface.clone();
                    move || can_bus_state(&iface)
                })
                .await
                .ok()
                .flatten();
                match state.as_deref() {
                    Some("ERROR-ACTIVE") | None => {} // healthy or unknown
                    Some(bad) => {
                        tracing::error!(
                            "[{}] CAN bus degraded → {} — exiting (systemd will restart). \
                             Fix: power on motors, then restart interface",
                            monitor_iface,
                            bad,
                        );
                        return Err::<(), _>(anyhow::anyhow!("CAN bus {}", bad));
                    }
                }
            }
        });

        // CAN → MoQ broadcast task (state fan-out to all subscribers)
        // Batches all available CAN frames into a single MoQ group to reduce
        // QUIC stream overhead (one group per batch instead of one per frame).
        let can_to_moq = tokio::spawn(async move {
            let mut batch_buf = Vec::with_capacity(1024);
            loop {
                // Wait for at least one frame
                let first = match can_read_rx.recv().await {
                    Some(frame) => frame,
                    None => break,
                };

                batch_buf.clear();
                batch_buf.extend_from_slice(&wire::encode(&first));

                // Drain all immediately available frames into the same batch
                while let Ok(frame) = can_read_rx.try_recv() {
                    batch_buf.extend_from_slice(&wire::encode(&frame));
                }

                // Single MoQ group write for the entire batch
                state_writer.write(batch_buf.clone());
            }
            can_read_rx
        });

        // MoQ → CAN task (command receiver with retry)
        // Races against the bus monitor — if the bus degrades, we exit immediately.
        let cmd_path = format!("{}/commands", path);
        let cmd_loop = async {
            loop {
                tracing::info!("Waiting for command publisher on {}/commands...", path);

                let cmd_sub = match builder.clone().path(&cmd_path).connect_subscriber().await {
                    Ok(sub) => sub,
                    Err(e) => {
                        tracing::warn!("Command subscriber connect error: {}, retrying...", e);
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                };

                let (cmd_reader, cmd_sub) =
                    match tokio::time::timeout(Duration::from_secs(5), async {
                        let mut sub = cmd_sub;
                        let result = sub.subscribe_track("can").await;
                        (result, sub)
                    })
                    .await
                    {
                        Ok((Ok(Some(reader)), sub)) => {
                            tracing::info!("Command subscriber connected on {}/commands", path);
                            (reader, sub)
                        }
                        Ok((Ok(None), sub)) => {
                            tracing::debug!("Command broadcast ended, retrying...");
                            drop(sub);
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            continue;
                        }
                        Ok((Err(e), sub)) => {
                            tracing::debug!("Command subscribe error: {}, retrying...", e);
                            drop(sub);
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            continue;
                        }
                        Err(_) => {
                            // Timeout — cmd_sub was moved into the async block and consumed
                            tracing::debug!("No command publisher yet, retrying...");
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            continue;
                        }
                    };
                let mut cmd_reader = cmd_reader;
                let _cmd_sub = cmd_sub; // keep alive while reading

                // Read commands until stream ends
                let mut pending = Vec::new();
                loop {
                    match cmd_reader.read().await {
                        Ok(Some(data)) => {
                            pending.extend_from_slice(&data);

                            while pending.len() >= 6 {
                                match wire::encoded_size(&pending) {
                                    Ok(frame_size) if pending.len() >= frame_size => {
                                        match wire::decode(&pending) {
                                            Ok((frame, consumed)) => {
                                                if can_write_tx.send(frame).await.is_err() {
                                                    tracing::error!("CAN writer thread died");
                                                    break;
                                                }
                                                pending.drain(..consumed);
                                            }
                                            Err(e) => {
                                                tracing::error!("Frame decode error: {}", e);
                                                pending.clear();
                                                break;
                                            }
                                        }
                                    }
                                    Ok(_) => break,
                                    Err(e) => {
                                        tracing::error!("Frame size error: {}", e);
                                        pending.clear();
                                        break;
                                    }
                                }
                            }
                        }
                        Ok(None) => {
                            tracing::info!("Command stream ended, will reconnect...");
                            break;
                        }
                        Err(e) => {
                            tracing::warn!("Command read error: {}, will reconnect...", e);
                            break;
                        }
                    }
                }

                // Command stream ended — loop back to reconnect for next operator
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        };

        // Race command loop against bus health monitor
        let result = tokio::select! {
            _ = cmd_loop => Ok(()),
            monitor_result = bus_monitor => {
                match monitor_result {
                    Ok(Err(e)) => Err(e),  // bus degraded
                    Ok(Ok(())) => Ok(()),
                    Err(e) => Err(anyhow::anyhow!("Bus monitor panicked: {}", e)),
                }
            }
        };

        // Recover the receiver for potential reuse
        can_to_moq.abort();
        match can_to_moq.await {
            Ok(rx) => {
                self.can_read_rx = Some(rx);
            }
            Err(_) => {} // aborted, can't recover
        }

        result
    }
}

// --- CAN reader/writer threads (same as can_server.rs) ---

fn can_reader_thread_fd(
    socket: Arc<socketcan::CanFdSocket>,
    tx: tokio::sync::mpsc::Sender<AnyCanFrame>,
    write_count: Arc<AtomicU64>,
) {
    let mut last_read = Instant::now();
    let mut would_blocks: u32 = 0;
    let mut timed_outs: u32 = 0;
    let mut writes_at_gap_start: u64 = 0;
    loop {
        match socket.read_frame() {
            Ok(frame) => {
                let gap = last_read.elapsed();
                let writes_now = write_count.load(Ordering::Relaxed);
                let writes_during = writes_now - writes_at_gap_start;
                if gap > Duration::from_millis(50) && writes_during >= 3 {
                    tracing::warn!(
                        "CAN response delay: {:.1}ms ({} timed_out, {} would_block, {} writes during gap)",
                        gap.as_secs_f64() * 1000.0,
                        timed_outs,
                        would_blocks,
                        writes_during,
                    );
                }
                last_read = Instant::now();
                would_blocks = 0;
                timed_outs = 0;
                writes_at_gap_start = writes_now;
                let any_frame = match frame {
                    socketcan::CanAnyFrame::Normal(f) => match CanFrame::try_from(f) {
                        Ok(cf) => AnyCanFrame::Can(cf),
                        Err(e) => {
                            tracing::warn!("CAN frame conversion error: {}", e);
                            continue;
                        }
                    },
                    socketcan::CanAnyFrame::Fd(f) => match CanFdFrame::try_from(f) {
                        Ok(cf) => AnyCanFrame::CanFd(cf),
                        Err(e) => {
                            tracing::warn!("CAN FD frame conversion error: {}", e);
                            continue;
                        }
                    },
                    socketcan::CanAnyFrame::Remote(_) => continue,
                    socketcan::CanAnyFrame::Error(e) => {
                        tracing::warn!("CAN bus error frame: {:?}", e);
                        continue;
                    }
                };
                if tx.blocking_send(any_frame).is_err() {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                would_blocks += 1;
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                timed_outs += 1;
                continue;
            }
            Err(e) => {
                tracing::warn!("CAN read error (ignoring): {}", e);
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn can_reader_thread_std(
    socket: Arc<socketcan::CanSocket>,
    tx: tokio::sync::mpsc::Sender<AnyCanFrame>,
    write_count: Arc<AtomicU64>,
) {
    let mut last_read = Instant::now();
    let mut would_blocks: u32 = 0;
    let mut timed_outs: u32 = 0;
    let mut writes_at_gap_start: u64 = 0;
    loop {
        match socket.read_frame() {
            Ok(frame) => {
                let gap = last_read.elapsed();
                let writes_now = write_count.load(Ordering::Relaxed);
                let writes_during = writes_now - writes_at_gap_start;
                if gap > Duration::from_millis(50) && writes_during >= 3 {
                    tracing::warn!(
                        "CAN response delay: {:.1}ms ({} timed_out, {} would_block, {} writes during gap)",
                        gap.as_secs_f64() * 1000.0,
                        timed_outs,
                        would_blocks,
                        writes_during,
                    );
                }
                last_read = Instant::now();
                would_blocks = 0;
                timed_outs = 0;
                writes_at_gap_start = writes_now;
                let any_frame = match CanFrame::try_from(frame) {
                    Ok(cf) => AnyCanFrame::Can(cf),
                    Err(e) => {
                        tracing::warn!("CAN frame conversion error: {}", e);
                        continue;
                    }
                };
                if tx.blocking_send(any_frame).is_err() {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                would_blocks += 1;
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                timed_outs += 1;
                continue;
            }
            Err(e) => {
                tracing::warn!("CAN read error (ignoring): {}", e);
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn can_writer_thread_fd(
    socket: Arc<socketcan::CanFdSocket>,
    mut rx: tokio::sync::mpsc::Receiver<AnyCanFrame>,
    write_count: Arc<AtomicU64>,
) {
    let write_count_ref = &write_count;
    let mut last_recv = Instant::now();
    let write_fn = |frame: &AnyCanFrame| {
        for attempt in 0..4u32 {
            let result = match frame {
                AnyCanFrame::Can(f) => match socketcan::CanFrame::try_from(f) {
                    Ok(sf) => socket.write_frame(&sf).map(|_| ()),
                    Err(e) => {
                        tracing::warn!("CAN frame conversion error on write: {}", e);
                        return;
                    }
                },
                AnyCanFrame::CanFd(f) => match socketcan::CanFdFrame::try_from(f) {
                    Ok(sf) => socket.write_frame(&sf).map(|_| ()),
                    Err(e) => {
                        tracing::warn!("CAN FD frame conversion error on write: {}", e);
                        return;
                    }
                },
            };
            if result.is_ok() {
                write_count_ref.fetch_add(1, Ordering::Relaxed);
                return;
            }
            let err = result.unwrap_err();
            if err.raw_os_error() == Some(105) && attempt < 3 {
                std::thread::sleep(Duration::from_micros(100));
                continue;
            }
            tracing::warn!("CAN write error (dropping frame): {}", err);
            return;
        }
    };

    while let Some(frame) = rx.blocking_recv() {
        let recv_gap = last_recv.elapsed();
        last_recv = Instant::now();
        if recv_gap > Duration::from_millis(50) {
            tracing::warn!(
                "CAN writer: {:.1}ms gap between commands from network",
                recv_gap.as_secs_f64() * 1000.0,
            );
        }
        let write_start = Instant::now();
        write_fn(&frame);
        let write_dur = write_start.elapsed();
        if write_dur > Duration::from_millis(5) {
            tracing::warn!(
                "CAN writer: write_frame took {:.1}ms",
                write_dur.as_secs_f64() * 1000.0,
            );
        }
    }
}

fn can_writer_thread_std(
    socket: Arc<socketcan::CanSocket>,
    mut rx: tokio::sync::mpsc::Receiver<AnyCanFrame>,
    write_count: Arc<AtomicU64>,
) {
    let write_count_ref = &write_count;
    let write_fn = |frame: &AnyCanFrame| {
        for attempt in 0..4u32 {
            let result = match frame {
                AnyCanFrame::Can(f) => match socketcan::CanFrame::try_from(f) {
                    Ok(sf) => socket.write_frame(&sf).map(|_| ()),
                    Err(e) => {
                        tracing::warn!("CAN frame conversion error on write: {}", e);
                        return;
                    }
                },
                AnyCanFrame::CanFd(_) => {
                    tracing::warn!("CAN FD frame on standard CAN socket, dropping");
                    return;
                }
            };
            if result.is_ok() {
                write_count_ref.fetch_add(1, Ordering::Relaxed);
                return;
            }
            let err = result.unwrap_err();
            if err.raw_os_error() == Some(105) && attempt < 3 {
                std::thread::sleep(Duration::from_micros(100));
                continue;
            }
            tracing::warn!("CAN write error (dropping frame): {}", err);
            return;
        }
    };

    let mut last_recv = Instant::now();
    while let Some(frame) = rx.blocking_recv() {
        let recv_gap = last_recv.elapsed();
        last_recv = Instant::now();
        if recv_gap > Duration::from_millis(50) {
            tracing::warn!(
                "CAN writer: {:.1}ms gap between commands from network",
                recv_gap.as_secs_f64() * 1000.0,
            );
        }
        let write_start = Instant::now();
        write_fn(&frame);
        let write_dur = write_start.elapsed();
        if write_dur > Duration::from_millis(5) {
            tracing::warn!(
                "CAN writer: write_frame took {:.1}ms",
                write_dur.as_secs_f64() * 1000.0,
            );
        }
    }
}
