//! CAN bridge server - bridges local CAN interface to remote clients over iroh P2P.
//!
//! This module provides a server that exposes a local CAN interface over the network,
//! allowing remote clients to send and receive CAN frames.
//!
//! Architecture: 2 persistent OS threads (reader + writer) communicate with async
//! connection tasks via channels. No extra tokio runtimes, no Mutex held across
//! awaits. CAN-to-network writes are batched for throughput.

use anyhow::Result;
use socketcan::{EmbeddedFrame, Frame, Socket};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::can::{CanFdFrame, CanFrame};
use crate::can_types::{wire, AnyCanFrame};
use crate::iroh::{IrohConnection, IrohServerBuilder};

/// A server that bridges a local CAN interface to remote clients over iroh P2P.
pub struct CanServer {
    server_id: String,
    /// Sender for frames to write to CAN interface (bounded tokio channel).
    can_write_tx: tokio::sync::mpsc::Sender<AnyCanFrame>,
    /// Receiver for write-ACKs from the writer thread (one ACK per frame written to CAN).
    write_ack_rx: std::sync::Mutex<Option<tokio::sync::mpsc::Receiver<()>>>,
    /// Receiver for frames read from CAN interface.
    /// Wrapped in `Mutex<Option<>>` so ownership can be transferred to/from connection tasks.
    /// The Mutex is held only for a quick `take()`/`replace()` swap, never across awaits.
    can_read_rx: std::sync::Mutex<Option<tokio::sync::mpsc::Receiver<AnyCanFrame>>>,
    endpoint: Arc<crate::iroh::IrohServer>,
}

/// Reader thread: opens a socketcan socket and pushes frames into a tokio mpsc channel.
///
/// Uses `blocking_send` so no tokio runtime is needed on this thread.
fn can_reader_thread(
    interface: String,
    enable_fd: bool,
    tx: tokio::sync::mpsc::Sender<AnyCanFrame>,
    init_tx: std::sync::mpsc::SyncSender<Result<()>>,
    writer_busy: Arc<AtomicU64>,
) {
    let timeout = Duration::from_millis(100);

    if enable_fd {
        let socket = match socketcan::CanFdSocket::open(&interface) {
            Ok(s) => s,
            Err(e) => {
                let _ = init_tx.send(Err(anyhow::anyhow!(
                    "Failed to open CAN FD reader socket on {}: {}",
                    interface,
                    e
                )));
                return;
            }
        };
        if let Err(e) = socket.set_read_timeout(timeout) {
            let _ = init_tx.send(Err(anyhow::anyhow!("Failed to set read timeout: {}", e)));
            return;
        }
        let _ = init_tx.send(Ok(()));

        let mut last_read_time: Option<Instant> = None;
        let mut consecutive_timeouts: u64 = 0;
        let mut writer_writes_at_gap_start: u64 = 0;
        loop {
            let read_start = Instant::now();
            match socket.read_frame() {
                Ok(frame) => {
                    let read_elapsed = read_start.elapsed();
                    let now = Instant::now();
                    let (frame_type, raw_id, data_len) = match &frame {
                        socketcan::CanAnyFrame::Normal(f) => ("CAN", f.raw_id(), f.data().len()),
                        socketcan::CanAnyFrame::Fd(f) => ("FD", f.raw_id(), f.data().len()),
                        socketcan::CanAnyFrame::Remote(f) => ("RTR", f.raw_id(), f.data().len()),
                        socketcan::CanAnyFrame::Error(f) => ("ERR", f.raw_id(), f.data().len()),
                    };
                    if let Some(prev) = last_read_time {
                        let gap = now.duration_since(prev);
                        if gap > Duration::from_millis(50) {
                            let writer_writes_now = writer_busy.load(Ordering::Relaxed);
                            let writes_during_gap = writer_writes_now - writer_writes_at_gap_start;
                            tracing::warn!(
                                "CAN reader: {:.1}ms gap (read_frame blocked {:.1}ms), {} timeouts, {} writes during gap, first frame: {} id=0x{:x} len={}",
                                gap.as_secs_f64() * 1000.0,
                                read_elapsed.as_secs_f64() * 1000.0,
                                consecutive_timeouts,
                                writes_during_gap,
                                frame_type,
                                raw_id,
                                data_len,
                            );
                        }
                    }
                    last_read_time = Some(now);
                    consecutive_timeouts = 0;
                    writer_writes_at_gap_start = writer_busy.load(Ordering::Relaxed);

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
                    let send_start = Instant::now();
                    if tx.blocking_send(any_frame).is_err() {
                        break; // Receiver dropped
                    }
                    let send_elapsed = send_start.elapsed();
                    if send_elapsed > Duration::from_millis(1) {
                        tracing::warn!(
                            "CAN reader: blocking_send took {:.1}ms (channel backpressure)",
                            send_elapsed.as_secs_f64() * 1000.0
                        );
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    consecutive_timeouts += 1;
                    continue;
                }
                Err(e) => {
                    tracing::warn!("CAN read error (ignoring): {}", e);
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    } else {
        let socket = match socketcan::CanSocket::open(&interface) {
            Ok(s) => s,
            Err(e) => {
                let _ = init_tx.send(Err(anyhow::anyhow!(
                    "Failed to open CAN reader socket on {}: {}",
                    interface,
                    e
                )));
                return;
            }
        };
        if let Err(e) = socket.set_read_timeout(timeout) {
            let _ = init_tx.send(Err(anyhow::anyhow!("Failed to set read timeout: {}", e)));
            return;
        }
        let _ = init_tx.send(Ok(()));

        let mut last_read_time: Option<Instant> = None;
        let mut consecutive_timeouts: u64 = 0;
        let mut writer_writes_at_gap_start: u64 = 0;
        loop {
            let read_start = Instant::now();
            match socket.read_frame() {
                Ok(frame) => {
                    let read_elapsed = read_start.elapsed();
                    let now = Instant::now();
                    if let Some(prev) = last_read_time {
                        let gap = now.duration_since(prev);
                        if gap > Duration::from_millis(50) {
                            let writer_writes_now = writer_busy.load(Ordering::Relaxed);
                            let writes_during_gap = writer_writes_now - writer_writes_at_gap_start;
                            tracing::warn!(
                                "CAN reader: {:.1}ms gap (read_frame blocked {:.1}ms), {} timeouts, {} writes during gap, first frame: CAN id=0x{:x} len={}",
                                gap.as_secs_f64() * 1000.0,
                                read_elapsed.as_secs_f64() * 1000.0,
                                consecutive_timeouts,
                                writes_during_gap,
                                frame.raw_id(),
                                frame.data().len(),
                            );
                        }
                    }
                    last_read_time = Some(now);
                    consecutive_timeouts = 0;
                    writer_writes_at_gap_start = writer_busy.load(Ordering::Relaxed);

                    let any_frame = match CanFrame::try_from(frame) {
                        Ok(cf) => AnyCanFrame::Can(cf),
                        Err(e) => {
                            tracing::warn!("CAN frame conversion error: {}", e);
                            continue;
                        }
                    };
                    let send_start = Instant::now();
                    if tx.blocking_send(any_frame).is_err() {
                        break; // Receiver dropped
                    }
                    let send_elapsed = send_start.elapsed();
                    if send_elapsed > Duration::from_millis(1) {
                        tracing::warn!(
                            "CAN reader: blocking_send took {:.1}ms (channel backpressure)",
                            send_elapsed.as_secs_f64() * 1000.0
                        );
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    consecutive_timeouts += 1;
                    continue;
                }
                Err(e) => {
                    tracing::warn!("CAN read error (ignoring): {}", e);
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }
}

/// Writer thread: receives frames from a tokio mpsc channel and writes them to CAN.
///
/// When [`JITTER_BUFFER_ENABLED`] is `true`, frames are buffered into batches and
/// played out at a steady measured rate to absorb network jitter (follower arm).
/// When `false`, frames are written immediately as they arrive (leader arm).
fn can_writer_thread(
    interface: String,
    enable_fd: bool,
    jitter_buffer: bool,
    mut rx: tokio::sync::mpsc::Receiver<AnyCanFrame>,
    init_tx: std::sync::mpsc::SyncSender<Result<()>>,
    writer_busy: Arc<AtomicU64>,
    write_ack_tx: tokio::sync::mpsc::Sender<()>,
) {
    if enable_fd {
        let socket = match socketcan::CanFdSocket::open(&interface) {
            Ok(s) => s,
            Err(e) => {
                let _ = init_tx.send(Err(anyhow::anyhow!(
                    "Failed to open CAN FD writer socket on {}: {}",
                    interface,
                    e
                )));
                return;
            }
        };
        let _ = init_tx.send(Ok(()));

        let writer_busy_ref = &writer_busy;
        let ack_ref = &write_ack_tx;
        let write_fn = |frame: &AnyCanFrame| {
            let write_start = Instant::now();
            for attempt in 0..4u32 {
                let result = match frame {
                    AnyCanFrame::Can(f) => match socketcan::CanFrame::try_from(f) {
                        Ok(sf) => socket.write_frame(&sf).map(|_| ()),
                        Err(e) => {
                            tracing::warn!("CAN frame conversion error on write: {}", e);
                            let _ = ack_ref.blocking_send(());
                            return;
                        }
                    },
                    AnyCanFrame::CanFd(f) => match socketcan::CanFdFrame::try_from(f) {
                        Ok(sf) => socket.write_frame(&sf).map(|_| ()),
                        Err(e) => {
                            tracing::warn!("CAN FD frame conversion error on write: {}", e);
                            let _ = ack_ref.blocking_send(());
                            return;
                        }
                    },
                };
                if result.is_ok() {
                    writer_busy_ref.fetch_add(1, Ordering::Relaxed);
                    let elapsed = write_start.elapsed();
                    if elapsed > Duration::from_millis(2) {
                        tracing::warn!(
                            "CAN writer: single frame write took {:.1}ms",
                            elapsed.as_secs_f64() * 1000.0
                        );
                    }
                    let _ = ack_ref.blocking_send(());
                    return;
                }
                let err = result.unwrap_err();
                if err.raw_os_error() == Some(105) && attempt < 3 {
                    tracing::warn!("ENOBUFS retry {}/3", attempt + 1);
                    std::thread::sleep(Duration::from_micros(100));
                    continue;
                }
                tracing::warn!("CAN write error (dropping frame): {}", err);
                let _ = ack_ref.blocking_send(());
                return;
            }
        };

        if jitter_buffer {
            jitter_buffer_loop(rx, write_fn);
        } else {
            while let Some(frame) = rx.blocking_recv() {
                write_fn(&frame);
            }
        }
    } else {
        let socket = match socketcan::CanSocket::open(&interface) {
            Ok(s) => s,
            Err(e) => {
                let _ = init_tx.send(Err(anyhow::anyhow!(
                    "Failed to open CAN writer socket on {}: {}",
                    interface,
                    e
                )));
                return;
            }
        };
        let _ = init_tx.send(Ok(()));

        let writer_busy_ref = &writer_busy;
        let ack_ref = &write_ack_tx;
        let write_fn = |frame: &AnyCanFrame| {
            let write_start = Instant::now();
            for attempt in 0..4u32 {
                let result = match frame {
                    AnyCanFrame::Can(f) => match socketcan::CanFrame::try_from(f) {
                        Ok(sf) => socket.write_frame(&sf).map(|_| ()),
                        Err(e) => {
                            tracing::warn!("CAN frame conversion error on write: {}", e);
                            let _ = ack_ref.blocking_send(());
                            return;
                        }
                    },
                    AnyCanFrame::CanFd(_) => {
                        tracing::warn!("CAN FD frame on standard CAN socket, dropping");
                        let _ = ack_ref.blocking_send(());
                        return;
                    }
                };
                if result.is_ok() {
                    writer_busy_ref.fetch_add(1, Ordering::Relaxed);
                    let elapsed = write_start.elapsed();
                    if elapsed > Duration::from_millis(2) {
                        tracing::warn!(
                            "CAN writer: single frame write took {:.1}ms",
                            elapsed.as_secs_f64() * 1000.0
                        );
                    }
                    let _ = ack_ref.blocking_send(());
                    return;
                }
                let err = result.unwrap_err();
                if err.raw_os_error() == Some(105) && attempt < 3 {
                    tracing::warn!("ENOBUFS retry {}/3", attempt + 1);
                    std::thread::sleep(Duration::from_micros(100));
                    continue;
                }
                tracing::warn!("CAN write error (dropping frame): {}", err);
                let _ = ack_ref.blocking_send(());
                return;
            }
        };

        if jitter_buffer {
            jitter_buffer_loop(rx, write_fn);
        } else {
            while let Some(frame) = rx.blocking_recv() {
                write_fn(&frame);
            }
        }
    }
}

/// Batch gap: frames arriving within this window are grouped into one batch.
/// Must be wide enough to capture all motor frames in one control cycle
/// (8 motors arriving over the network may spread across several ms).
const BATCH_GAP: Duration = Duration::from_millis(10);

/// Maximum batches held in the jitter buffer.
const BUFFER_CAP: usize = 5;

/// Minimum playback interval clamp.
const MIN_INTERVAL: Duration = Duration::from_millis(10);

/// Maximum playback interval clamp.
const MAX_INTERVAL: Duration = Duration::from_millis(200);

/// Default playback interval (~30 Hz) used until we have measurements.
const DEFAULT_INTERVAL: Duration = Duration::from_millis(33);

/// EMA smoothing factor for interval estimation (weight given to new measurement).
const EMA_ALPHA: f64 = 0.2;

/// Buffer depth at which playback starts speeding up to drain the backlog.
/// Above this, playback interval is progressively shortened.
const BUFFER_CATCHUP_THRESHOLD: usize = 2;

/// Delay between individual CAN frame writes within a batch, to avoid
/// overflowing the kernel CAN socket buffer (ENOBUFS / os error 105).
const INTER_FRAME_DELAY: Duration = Duration::from_millis(1);

/// Number of consecutive regular-cadence multi-frame batches required to activate buffering.
const STREAMING_THRESHOLD: u32 = 15;

/// Minimum frames in a batch for it to count toward streaming detection.
/// Handshake sends 1-2 frames at a time; a real control cycle sends all motors.
const MIN_BATCH_SIZE_FOR_STREAMING: usize = 3;

/// Drain all immediately available frames from the channel into `current_batch`,
/// finalizing completed batches into `buffer`. No sleeping — just grabs what's
/// ready and returns. Returns `true` if the channel is disconnected.
fn drain_into_batches(
    rx: &mut tokio::sync::mpsc::Receiver<AnyCanFrame>,
    current_batch: &mut Vec<AnyCanFrame>,
    last_frame_time: &mut Option<Instant>,
    buffer: &mut VecDeque<Vec<AnyCanFrame>>,
    last_batch_arrival: &mut Option<Instant>,
    interval_estimate: &mut Duration,
    consecutive_regular: &mut u32,
    stats: &mut JitterStats,
    streaming_start: Option<Instant>,
    streaming_batch_count: &mut u64,
) -> bool {
    loop {
        match rx.try_recv() {
            Ok(frame) => {
                let now = Instant::now();
                // If gap since last frame > BATCH_GAP, finalize current batch
                if let Some(prev) = *last_frame_time {
                    if now.duration_since(prev) > BATCH_GAP && !current_batch.is_empty() {
                        finalize_batch(
                            buffer,
                            current_batch,
                            last_batch_arrival,
                            interval_estimate,
                            consecutive_regular,
                            stats,
                            streaming_start,
                            streaming_batch_count,
                        );
                    }
                }
                current_batch.push(frame);
                *last_frame_time = Some(now);
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                if !current_batch.is_empty() {
                    finalize_batch(
                        buffer,
                        current_batch,
                        last_batch_arrival,
                        interval_estimate,
                        consecutive_regular,
                        stats,
                        streaming_start,
                        streaming_batch_count,
                    );
                }
                return true;
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                // Nothing available right now — done draining
                return false;
            }
        }
    }
}

struct IntervalStats {
    median: f64,
    iqr: f64,
    p95: f64,
    p99: f64,
    min: f64,
    max: f64,
}

/// Statistics collected during jitter buffer operation.
struct JitterStats {
    /// Total batches received from the network.
    batches_received: u64,
    /// Batches played out to CAN (includes replays).
    batches_played: u64,
    /// Batches replayed because the buffer was empty.
    batches_replayed: u64,
    /// Batches dropped because the buffer was full.
    batches_dropped: u64,
    /// Inter-batch arrival intervals (for jitter calculation).
    arrival_intervals: Vec<f64>,
    /// Inter-playback intervals (for jitter calculation).
    playback_intervals: Vec<f64>,
    /// Peak buffer depth observed.
    max_buffer_depth: usize,
}

impl JitterStats {
    fn new() -> Self {
        Self {
            batches_received: 0,
            batches_played: 0,
            batches_replayed: 0,
            batches_dropped: 0,
            arrival_intervals: Vec::new(),
            playback_intervals: Vec::new(),
            max_buffer_depth: 0,
        }
    }

    fn record_arrival_interval(&mut self, interval: Duration) {
        self.arrival_intervals.push(interval.as_secs_f64() * 1000.0);
    }

    fn record_playback_interval(&mut self, interval: Duration) {
        self.playback_intervals.push(interval.as_secs_f64() * 1000.0);
    }

    fn print_summary(&self) {
        if self.batches_received == 0 {
            return;
        }

        tracing::warn!("=== Jitter Buffer Stats ===");
        tracing::warn!(
            "Batches: {} received, {} played, {} replayed, {} dropped",
            self.batches_received,
            self.batches_played,
            self.batches_replayed,
            self.batches_dropped,
        );
        tracing::warn!("Peak buffer depth: {}", self.max_buffer_depth);

        if let Some(s) = Self::compute_stats(&self.arrival_intervals) {
            tracing::warn!(
                "Arrival:  median={:.1}ms p95={:.1}ms p99={:.1}ms min={:.1}ms max={:.1}ms",
                s.median, s.p95, s.p99, s.min, s.max,
            );
        }
        if let Some(s) = Self::compute_stats(&self.playback_intervals) {
            tracing::warn!(
                "Playback: median={:.1}ms p95={:.1}ms p99={:.1}ms min={:.1}ms max={:.1}ms",
                s.median, s.p95, s.p99, s.min, s.max,
            );
        }

        if let (Some(arr), Some(play)) = (
            Self::compute_stats(&self.arrival_intervals),
            Self::compute_stats(&self.playback_intervals),
        ) {
            // Jitter reduction based on IQR (robust to outliers)
            if arr.iqr > 0.001 {
                let reduction = ((arr.iqr - play.iqr) / arr.iqr * 100.0).max(0.0);
                tracing::warn!(
                    "Jitter reduction: {:.0}% (arrival IQR={:.1}ms → playback IQR={:.1}ms)",
                    reduction, arr.iqr, play.iqr,
                );
            }
        }
    }

    fn percentile(sorted: &[f64], p: f64) -> f64 {
        if sorted.is_empty() {
            return 0.0;
        }
        let idx = (p / 100.0 * (sorted.len() - 1) as f64).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    }

    fn compute_stats(intervals: &[f64]) -> Option<IntervalStats> {
        if intervals.len() < 2 {
            return None;
        }
        let mut sorted = intervals.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());

        let median = Self::percentile(&sorted, 50.0);
        let p25 = Self::percentile(&sorted, 25.0);
        let p75 = Self::percentile(&sorted, 75.0);
        let p95 = Self::percentile(&sorted, 95.0);
        let p99 = Self::percentile(&sorted, 99.0);
        let min = sorted[0];
        let max = sorted[sorted.len() - 1];

        Some(IntervalStats {
            median,
            iqr: p75 - p25,
            p95,
            p99,
            min,
            max,
        })
    }
}

impl Drop for JitterStats {
    fn drop(&mut self) {
        self.print_summary();
    }
}

/// Write all frames in a batch with a small delay between each to avoid
/// overflowing the kernel CAN socket buffer (ENOBUFS).
fn write_batch(batch: &[AnyCanFrame], write_frame: &mut impl FnMut(&AnyCanFrame)) {
    let batch_start = Instant::now();
    for (i, f) in batch.iter().enumerate() {
        write_frame(f);
        if i + 1 < batch.len() {
            std::thread::sleep(INTER_FRAME_DELAY);
        }
    }
    let batch_elapsed = batch_start.elapsed();
    if batch_elapsed > Duration::from_millis(5) {
        tracing::warn!(
            "CAN writer: write_batch took {:.1}ms ({} frames)",
            batch_elapsed.as_secs_f64() * 1000.0,
            batch.len()
        );
    }
}

/// Core jitter-buffer loop shared by both FD and non-FD writer paths.
///
/// Starts in **passthrough mode** where frames are written to CAN immediately.
/// Once [`STREAMING_THRESHOLD`] consecutive batches arrive at a regular cadence
/// (within [`MIN_INTERVAL`]–[`MAX_INTERVAL`]), switches to **buffered mode**
/// where batches are held in a ring buffer and played out at a steady measured
/// rate. Falls back to passthrough when traffic goes idle (no batch for 2×
/// the estimated interval). Prints jitter statistics on exit.
fn jitter_buffer_loop(
    mut rx: tokio::sync::mpsc::Receiver<AnyCanFrame>,
    mut write_frame: impl FnMut(&AnyCanFrame),
) {
    let mut buffer: VecDeque<Vec<AnyCanFrame>> = VecDeque::new();
    let mut current_batch: Vec<AnyCanFrame> = Vec::new();
    let mut last_frame_time: Option<Instant> = None;
    let mut last_batch_arrival: Option<Instant> = None;
    let mut interval_estimate = DEFAULT_INTERVAL;
    let mut last_play_time: Option<Instant> = None;
    let mut last_played_batch: Option<Vec<AnyCanFrame>> = None;
    let mut consecutive_regular: u32 = 0;
    let mut streaming = false;
    let mut stats = JitterStats::new();
    // Running average: track start time and count since streaming activated
    let mut streaming_start: Option<Instant> = None;
    let mut streaming_batch_count: u64 = 0;
    let mut last_stats_time = Instant::now();

    loop {
        // --- Block until the next frame arrives ---
        let frame = match rx.blocking_recv() {
            Some(f) => f,
            None => return,
        };
        let now = Instant::now();
        current_batch.push(frame);
        last_frame_time = Some(now);

        // --- Drain all immediately available frames ---
        let mut disconnected = drain_into_batches(
            &mut rx,
            &mut current_batch,
            &mut last_frame_time,
            &mut buffer,
            &mut last_batch_arrival,
            &mut interval_estimate,
            &mut consecutive_regular,
            &mut stats,
            streaming_start,
            &mut streaming_batch_count,
        );

        // In streaming mode, wait BATCH_GAP for remaining control cycle frames.
        // Motor frames (8 per cycle) may arrive slightly spread across a few ms.
        // In passthrough mode, finalize immediately for low-latency handshake.
        if !current_batch.is_empty() {
            if streaming && !disconnected {
                std::thread::sleep(BATCH_GAP);
                let disc = drain_into_batches(
                    &mut rx,
                    &mut current_batch,
                    &mut last_frame_time,
                    &mut buffer,
                    &mut last_batch_arrival,
                    &mut interval_estimate,
                    &mut consecutive_regular,
                    &mut stats,
                    streaming_start,
                    &mut streaming_batch_count,
                );
                if disc {
                    disconnected = true;
                }
            }
            finalize_batch(
                &mut buffer,
                &mut current_batch,
                &mut last_batch_arrival,
                &mut interval_estimate,
                &mut consecutive_regular,
                &mut stats,
                streaming_start,
                &mut streaming_batch_count,
            );
        }

        // --- Decide mode: activate streaming after enough regular batches ---
        if !streaming && consecutive_regular >= STREAMING_THRESHOLD {
            streaming = true;
            streaming_start = Some(Instant::now());
            streaming_batch_count = 0;
            tracing::warn!(
                "Jitter buffer activated (interval estimate: {:.1}ms)",
                interval_estimate.as_secs_f64() * 1000.0
            );
        }

        if !streaming {
            // Passthrough: write all buffered batches immediately
            while let Some(batch) = buffer.pop_front() {
                write_batch(&batch, &mut write_frame);
                last_played_batch = Some(batch);
            }
            last_play_time = Some(Instant::now());

            if disconnected {
                return;
            }
            continue;
        }

        // --- Streaming / buffered mode ---
        loop {
            // Track buffer depth
            if buffer.len() > stats.max_buffer_depth {
                stats.max_buffer_depth = buffer.len();
            }

            // Playback: write one batch per interval tick. When buffer is
            // building up, shorten the interval to drain it smoothly.
            let now = Instant::now();
            let effective_interval = if buffer.len() > BUFFER_CATCHUP_THRESHOLD {
                // Scale down: depth 3→75%, 4→50%, 5→25% of interval
                let excess = buffer.len() - BUFFER_CATCHUP_THRESHOLD;
                let scale = 1.0 / (1.0 + excess as f64);
                Duration::from_secs_f64(interval_estimate.as_secs_f64() * scale)
            } else {
                interval_estimate
            };
            let should_play = match last_play_time {
                Some(t) => now.duration_since(t) >= effective_interval,
                None => !buffer.is_empty(),
            };

            if should_play {
                if let Some(batch) = buffer.pop_front() {
                    write_batch(&batch, &mut write_frame);
                    if let Some(prev) = last_play_time {
                        stats.record_playback_interval(now.duration_since(prev));
                    }
                    stats.batches_played += 1;
                    last_played_batch = Some(batch);
                    last_play_time = Some(now);
                } else if let Some(ref batch) = last_played_batch {
                    tracing::debug!(
                        "Jitter buffer empty, replaying last batch ({} frames)",
                        batch.len()
                    );
                    write_batch(batch, &mut write_frame);
                    if let Some(prev) = last_play_time {
                        stats.record_playback_interval(now.duration_since(prev));
                    }
                    stats.batches_replayed += 1;
                    stats.batches_played += 1;
                    last_play_time = Some(now);
                }
            }

            if disconnected {
                // Flush remaining
                while let Some(batch) = buffer.pop_front() {
                    for f in &batch {
                        write_frame(f);
                    }
                }

                return;
            }

            // Periodic stats log (every 30s)
            if now.duration_since(last_stats_time) > Duration::from_secs(30) {
                stats.print_summary();
                last_stats_time = now;
            }

            // Deactivate after 5s idle (client likely disconnected or stopped)
            if let Some(last_arrival) = last_batch_arrival {
                if now.duration_since(last_arrival) > Duration::from_secs(5) {
                    streaming = false;
                    consecutive_regular = 0;
                    streaming_start = None;
                    streaming_batch_count = 0;
                    while let Some(batch) = buffer.pop_front() {
                        for f in &batch {
                            write_frame(f);
                        }
                    }
                    last_played_batch = None;
                    last_play_time = None;
                    tracing::warn!("Jitter buffer deactivated (5s idle)");
                    break;
                }
            }

            // Sleep until next playback tick, then drain any new data
            let wait = match last_play_time {
                Some(t) => effective_interval.saturating_sub(now.duration_since(t)),
                None => effective_interval,
            };
            if wait > Duration::from_millis(1) {
                std::thread::sleep(wait - Duration::from_millis(1));
            }

            // Drain everything available
            let disc = drain_into_batches(
                &mut rx,
                &mut current_batch,
                &mut last_frame_time,
                &mut buffer,
                &mut last_batch_arrival,
                &mut interval_estimate,
                &mut consecutive_regular,
                &mut stats,
                streaming_start,
                &mut streaming_batch_count,
            );
            // Finalize any partial batch that's been sitting longer than BATCH_GAP
            if !current_batch.is_empty() {
                if let Some(prev) = last_frame_time {
                    if Instant::now().duration_since(prev) > BATCH_GAP {
                        finalize_batch(
                            &mut buffer,
                            &mut current_batch,
                            &mut last_batch_arrival,
                            &mut interval_estimate,
                            &mut consecutive_regular,
                            &mut stats,
                            streaming_start,
                            &mut streaming_batch_count,
                        );
                    }
                }
            }
            if disc {
                while let Some(batch) = buffer.pop_front() {
                    for f in &batch {
                        write_frame(f);
                    }
                }
                return;
            }
        }
    }
}

/// Finalize the current batch: push it into the buffer, update interval estimate,
/// track consecutive regular arrivals, and enforce the buffer capacity.
fn finalize_batch(
    buffer: &mut VecDeque<Vec<AnyCanFrame>>,
    current_batch: &mut Vec<AnyCanFrame>,
    last_batch_arrival: &mut Option<Instant>,
    interval_estimate: &mut Duration,
    consecutive_regular: &mut u32,
    stats: &mut JitterStats,
    streaming_start: Option<Instant>,
    streaming_batch_count: &mut u64,
) {
    let now = Instant::now();
    let batch = std::mem::take(current_batch);

    stats.batches_received += 1;

    tracing::debug!(
        "Batch complete: {} frames, buffer depth: {}",
        batch.len(),
        buffer.len()
    );

    // Update interval estimate and streaming detection
    if let Some(prev) = *last_batch_arrival {
        let measured = now.duration_since(prev);
        stats.record_arrival_interval(measured);

        if measured >= MIN_INTERVAL && measured <= MAX_INTERVAL {
            if batch.len() >= MIN_BATCH_SIZE_FOR_STREAMING {
                *consecutive_regular += 1;
            } else {
                *consecutive_regular = 0;
            }

            // Pre-streaming: use EMA for detection phase
            if streaming_start.is_none() {
                let est = interval_estimate.as_secs_f64();
                let meas = measured.as_secs_f64();
                let new_est = est * (1.0 - EMA_ALPHA) + meas * EMA_ALPHA;
                *interval_estimate = Duration::from_secs_f64(new_est);
            }
        } else {
            *consecutive_regular = 0;
        }
    }
    *last_batch_arrival = Some(now);

    // Once streaming, use running average (total_elapsed / total_batches)
    // for a rock-solid rate immune to jitter.
    *streaming_batch_count += 1;
    if let Some(start) = streaming_start {
        let elapsed = now.duration_since(start);
        if *streaming_batch_count > 1 {
            let avg = elapsed.as_secs_f64() / *streaming_batch_count as f64;
            let clamped = avg.clamp(MIN_INTERVAL.as_secs_f64(), MAX_INTERVAL.as_secs_f64());
            *interval_estimate = Duration::from_secs_f64(clamped);
        }
    }

    // Enforce buffer cap — drop oldest if full
    if buffer.len() >= BUFFER_CAP {
        let dropped = buffer.pop_front().unwrap();
        stats.batches_dropped += 1;
        tracing::debug!(
            "Jitter buffer full, dropping oldest batch ({} frames)",
            dropped.len()
        );
    }

    buffer.push_back(batch);
}

impl CanServer {
    /// Create a new CAN bridge server.
    ///
    /// Spawns two OS threads (reader + writer) that access the CAN interface directly
    /// via socketcan. These threads communicate with async connection tasks via channels.
    ///
    /// # Arguments
    /// * `interface` - CAN interface name (e.g., "can0", "vcan0")
    /// * `enable_fd` - Enable CAN FD support
    /// * `jitter_buffer` - Enable jitter buffer in writer thread (use `true` for
    ///   follower arms to smooth network jitter, `false` for leader arms)
    /// * `identity_path` - Optional path to save/load server identity
    pub async fn new(
        interface: &str,
        enable_fd: bool,
        jitter_buffer: bool,
        identity_path: Option<&str>,
    ) -> Result<Self> {
        // CAN→Network channel (small to avoid batching stale responses)
        let (can_read_tx, can_read_rx) = tokio::sync::mpsc::channel::<AnyCanFrame>(16);
        // Network→CAN channel — capacity 1 so backpressure propagates immediately
        // to the client via QUIC flow control.
        let (can_write_tx, can_write_rx) = tokio::sync::mpsc::channel::<AnyCanFrame>(1);

        // Shared counter: writer increments on each successful CAN write,
        // reader reads it during gaps to see if writes were happening concurrently.
        let writer_busy = Arc::new(AtomicU64::new(0));

        // Spawn reader thread
        let (reader_init_tx, reader_init_rx) = std::sync::mpsc::sync_channel::<Result<()>>(1);
        let iface_reader = interface.to_string();
        let writer_busy_reader = Arc::clone(&writer_busy);
        std::thread::Builder::new()
            .name(format!("can-read-{}", interface))
            .spawn(move || {
                can_reader_thread(iface_reader, enable_fd, can_read_tx, reader_init_tx, writer_busy_reader);
            })?;

        // Write-ACK channel: writer thread sends () after each CAN write completes.
        // The net→CAN task waits for the ACK before reading the next frame from QUIC.
        let (write_ack_tx, write_ack_rx) = tokio::sync::mpsc::channel::<()>(1);

        // Spawn writer thread
        let (writer_init_tx, writer_init_rx) = std::sync::mpsc::sync_channel::<Result<()>>(1);
        let iface_writer = interface.to_string();
        let writer_busy_writer = Arc::clone(&writer_busy);
        std::thread::Builder::new()
            .name(format!("can-write-{}", interface))
            .spawn(move || {
                can_writer_thread(iface_writer, enable_fd, jitter_buffer, can_write_rx, writer_init_tx, writer_busy_writer, write_ack_tx);
            })?;

        // Wait for both threads to initialize (propagate socket open errors)
        reader_init_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("CAN reader thread died during init"))??;
        writer_init_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("CAN writer thread died during init"))??;

        // Start iroh server
        let mut builder = IrohServerBuilder::new();
        if let Some(path) = identity_path {
            builder = builder.identity_path(path);
        }
        let server = builder.bind().await?;
        let server_id = server.id().to_string();

        Ok(Self {
            server_id,
            can_write_tx,
            write_ack_rx: std::sync::Mutex::new(Some(write_ack_rx)),
            can_read_rx: std::sync::Mutex::new(Some(can_read_rx)),
            endpoint: Arc::new(server),
        })
    }

    /// Get the server's endpoint ID (share this with clients).
    pub fn id(&self) -> &str {
        &self.server_id
    }

    /// Run the bridge server (blocks forever, handling connections).
    ///
    /// When a new client connects, any existing connection is automatically
    /// terminated to allow reconnection. The CAN read receiver is recovered
    /// from the old connection task before being handed to the new one.
    pub async fn run(&self) -> Result<()> {
        tracing::info!("CAN bridge server running. ID: {}", self.server_id);

        type ConnState = (
            tokio::sync::mpsc::Receiver<AnyCanFrame>,
            tokio::sync::mpsc::Receiver<()>,
        );
        let mut current_conn: Option<(
            CancellationToken,
            tokio::task::JoinHandle<ConnState>,
        )> = None;

        loop {
            let conn = match self.endpoint.accept().await? {
                Some(c) => c,
                None => continue,
            };

            tracing::info!("Client connected: {}", conn.remote_id());

            // Cancel previous connection and recover the receivers
            if let Some((cancel, handle)) = current_conn.take() {
                tracing::info!("New client connected, closing previous connection");
                cancel.cancel();
                match handle.await {
                    Ok((rx, ack_rx)) => {
                        self.can_read_rx.lock().unwrap().replace(rx);
                        self.write_ack_rx.lock().unwrap().replace(ack_rx);
                    }
                    Err(e) => {
                        tracing::error!("Connection task panicked: {}", e);
                    }
                }
            }

            // Take receiver ownership for the new connection
            let rx = self
                .can_read_rx
                .lock()
                .unwrap()
                .take()
                .expect("CAN read receiver should be available");
            let ack_rx = self
                .write_ack_rx
                .lock()
                .unwrap()
                .take()
                .expect("Write ACK receiver should be available");

            let cancel = CancellationToken::new();
            let cancel_clone = cancel.clone();
            let write_tx = self.can_write_tx.clone();

            let handle = tokio::spawn(async move {
                let (result, rx, ack_rx) = handle_connection(conn, rx, write_tx, ack_rx, cancel_clone).await;
                if let Err(e) = &result {
                    tracing::error!("Connection error: {}", e);
                }
                tracing::info!("Client disconnected");
                (rx, ack_rx)
            });

            current_conn = Some((cancel, handle));
        }
    }

    /// Run the bridge server for a single connection, then return.
    pub async fn run_once(&self) -> Result<()> {
        tracing::info!(
            "CAN bridge server waiting for connection. ID: {}",
            self.server_id
        );

        loop {
            let conn = match self.endpoint.accept().await? {
                Some(c) => c,
                None => continue,
            };

            tracing::info!("Client connected: {}", conn.remote_id());

            let rx = self
                .can_read_rx
                .lock()
                .unwrap()
                .take()
                .expect("CAN read receiver should be available");
            let ack_rx = self
                .write_ack_rx
                .lock()
                .unwrap()
                .take()
                .expect("Write ACK receiver should be available");

            let write_tx = self.can_write_tx.clone();
            let cancel = CancellationToken::new();

            let (result, rx, ack_rx) = handle_connection(conn, rx, write_tx, ack_rx, cancel).await;

            // Put receivers back
            self.can_read_rx.lock().unwrap().replace(rx);
            self.write_ack_rx.lock().unwrap().replace(ack_rx);

            tracing::info!("Client disconnected");

            if let Err(e) = result {
                tracing::error!("Connection error: {}", e);
            }

            return Ok(());
        }
    }
}

/// Core bridge logic for a single connection.
///
/// The CAN read receiver is moved in and always moved back out, ensuring no
/// Mutex is held across awaits. CAN-to-network writes are batched: after
/// receiving the first frame, up to 63 additional ready frames are collected
/// and flushed in a single write.
async fn handle_connection(
    conn: IrohConnection,
    mut can_read_rx: tokio::sync::mpsc::Receiver<AnyCanFrame>,
    can_write_tx: tokio::sync::mpsc::Sender<AnyCanFrame>,
    mut write_ack_rx: tokio::sync::mpsc::Receiver<()>,
    cancel: CancellationToken,
) -> (
    Result<()>,
    tokio::sync::mpsc::Receiver<AnyCanFrame>,
    tokio::sync::mpsc::Receiver<()>,
) {
    let stream = match conn.accept_stream().await {
        Ok(s) => s,
        Err(e) => {
            return (
                Err(anyhow::anyhow!("Failed to accept stream: {}", e)),
                can_read_rx,
                write_ack_rx,
            );
        }
    };

    let (mut send, mut recv) = stream.split();

    // Drain stale frames
    let mut drained = 0;
    while can_read_rx.try_recv().is_ok() {
        drained += 1;
    }
    if drained > 0 {
        tracing::info!("Drained {} stale CAN frames from buffer", drained);
    }

    // Combine our cancellation with the connection's own token
    let conn_cancel = conn.cancellation_token();

    // CAN → Network task (with batching)
    let can_to_net_cancel = cancel.clone();
    let conn_cancel_clone = conn_cancel.clone();
    let can_to_net = tokio::spawn(async move {
        let mut batch_buf = Vec::with_capacity(1024);

        loop {
            batch_buf.clear();

            // Wait for first frame (or cancellation)
            let first = tokio::select! {
                _ = can_to_net_cancel.cancelled() => break,
                _ = conn_cancel_clone.cancelled() => break,
                frame = can_read_rx.recv() => match frame {
                    Some(f) => f,
                    None => break,
                }
            };

            // Encode first frame
            batch_buf.extend_from_slice(&wire::encode(&first));

            // Greedily collect more ready frames (up to 8 total)
            for _ in 1..8 {
                match can_read_rx.try_recv() {
                    Ok(frame) => batch_buf.extend_from_slice(&wire::encode(&frame)),
                    Err(_) => break,
                }
            }

            // Single write + flush for entire batch
            let flush_start = Instant::now();
            if send.write_all(&batch_buf).await.is_err() {
                break;
            }
            if send.flush().await.is_err() {
                break;
            }
            let flush_elapsed = flush_start.elapsed();
            if flush_elapsed > Duration::from_millis(5) {
                tracing::warn!(
                    "CAN→net: flush took {:.1}ms ({} bytes)",
                    flush_elapsed.as_secs_f64() * 1000.0,
                    batch_buf.len()
                );
            }
        }

        can_read_rx // Return receiver ownership
    });

    // Network → CAN (inline in this task)
    let mut buf = vec![0u8; 1024];
    let mut pending = Vec::new();
    let result = loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                break Ok(());
            }
            _ = conn_cancel.cancelled() => {
                break Ok(());
            }
            read_result = recv.read(&mut buf) => {
                match read_result {
                    Ok(Some(n)) if n > 0 => {
                        pending.extend_from_slice(&buf[..n]);

                        while pending.len() >= 6 {
                            match wire::encoded_size(&pending) {
                                Ok(frame_size) if pending.len() >= frame_size => {
                                    match wire::decode(&pending) {
                                        Ok((frame, consumed)) => {
                                            tracing::debug!(
                                                "Network -> CAN: ID={:x}, {} bytes",
                                                frame.id(),
                                                consumed
                                            );
                                            if can_write_tx.send(frame).await.is_err() {
                                                tracing::error!("CAN writer thread died");
                                                break;
                                            }
                                            // Wait for the writer to confirm the frame was
                                            // written to the CAN bus before reading more from
                                            // QUIC. This prevents write flooding the bus.
                                            if write_ack_rx.recv().await.is_none() {
                                                tracing::error!("CAN writer thread died (ACK channel closed)");
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
                                Ok(_) => break, // Need more data
                                Err(e) => {
                                    tracing::error!("Frame size error: {}", e);
                                    pending.clear();
                                    break;
                                }
                            }
                        }
                    }
                    Ok(Some(_)) => continue,
                    Ok(None) => {
                        tracing::info!("Client disconnected (stream closed)");
                        break Ok(());
                    }
                    Err(e) => {
                        break Err(anyhow::anyhow!("Network read error: {}", e));
                    }
                }
            }
        }
    };

    // Cancel the CAN-to-net task and recover the receiver
    cancel.cancel();
    let can_read_rx = match can_to_net.await {
        Ok(rx) => rx,
        Err(e) => {
            tracing::error!("CAN-to-net task panicked: {}", e);
            // Receiver is lost if the task panicked — this is unrecoverable
            // but we still need to return something. The caller will see the
            // panic error from run()'s handle.await.
            return (
                Err(anyhow::anyhow!("CAN-to-net task panicked: {}", e)),
                // Create dummy channels — run() will see the JoinError and
                // won't use these receivers anyway since the handle.await fails.
                tokio::sync::mpsc::channel(1).1,
                tokio::sync::mpsc::channel(1).1,
            );
        }
    };

    (result, can_read_rx, write_ack_rx)
}
