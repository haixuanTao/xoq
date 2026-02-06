//! MoQ CAN bridge server - bridges local CAN interface to remote clients via MoQ relay.
//!
//! Architecture: Reuses the same 2 persistent OS threads (reader + writer) as the iroh
//! CAN server, but bridges to MoQ tracks instead of QUIC streams.

use anyhow::Result;
use socketcan::Socket;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::can::{CanFdFrame, CanFrame};
use crate::can_types::{wire, AnyCanFrame};
use crate::moq::MoqStream;

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
        let (can_read_tx, can_read_rx) = tokio::sync::mpsc::channel::<AnyCanFrame>(16);
        let (can_write_tx, can_write_rx) = tokio::sync::mpsc::channel::<AnyCanFrame>(1);
        let write_count = Arc::new(AtomicU64::new(0));

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
    pub async fn run(&mut self, relay: &str, path: &str) -> Result<()> {
        tracing::info!(
            "MoQ CAN server connecting to {}/{} for interface {}...",
            relay,
            path,
            self.interface
        );

        let stream = MoqStream::accept_at(relay, path).await?;
        let stream = Arc::new(Mutex::new(stream));
        tracing::info!("MoQ CAN server connected to relay");

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

        // CAN → MoQ task
        let stream_writer = Arc::clone(&stream);
        let can_to_moq = tokio::spawn(async move {
            loop {
                match can_read_rx.recv().await {
                    Some(frame) => {
                        let encoded = wire::encode(&frame);
                        let mut s = stream_writer.lock().await;
                        s.write(encoded);
                    }
                    None => break,
                }
            }
            can_read_rx
        });

        // MoQ → CAN (main task)
        let mut pending = Vec::new();
        let result = loop {
            let data = {
                let mut s = stream.lock().await;
                s.read().await
            };
            match data {
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
                    tracing::info!("MoQ stream ended");
                    break Ok(());
                }
                Err(e) => {
                    break Err(anyhow::anyhow!("MoQ read error: {}", e));
                }
            }
        };

        // Recover the receiver for potential reuse
        match can_to_moq.await {
            Ok(rx) => {
                self.can_read_rx = Some(rx);
            }
            Err(e) => {
                tracing::error!("CAN-to-MoQ task panicked: {}", e);
            }
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
                    socketcan::CanAnyFrame::Remote(_) | socketcan::CanAnyFrame::Error(_) => {
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
