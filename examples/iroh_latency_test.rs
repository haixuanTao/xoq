//! Iroh P2P latency and jitter test
//!
//! Measures round-trip latency over iroh QUIC transport to diagnose
//! the write coalescing issue documented in CLAUDE.md.
//!
//! Usage:
//!   iroh_latency_test server [--key <path>] [--trace]
//!   iroh_latency_test client <server-id> [--count <n>] [--interval <ms>] [--mode <mode>]
//!
//! Modes:
//!   normal   - Standard write + read ping-pong (default)
//!   yield    - Add yield_now() after each write
//!   spawn    - Spawn write as separate task
//!   flush    - Call flush after each write
//!   datagram - Use QUIC datagrams (unreliable, no ACKs)
//!   trace    - Detailed tracing of message lengths through data path
//!
//! Example:
//!   Terminal 1: cargo run --example iroh_latency_test --features iroh -- server --trace
//!   Terminal 2: cargo run --example iroh_latency_test --features iroh -- client <id> --mode trace

use anyhow::Result;
use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use xoq::iroh::{IrohClientBuilder, IrohServerBuilder};

const DEFAULT_COUNT: usize = 100;
const DEFAULT_INTERVAL_MS: u64 = 12;
const PING_SIZE: usize = 8; // 8-byte sequence number

#[derive(Clone, Copy, Debug)]
enum WriteMode {
    Normal,
    Yield,
    Spawn,
    Flush,
    Datagram,
    Trace, // Detailed tracing with timestamps and message lengths
}

/// Trace message format: 24 bytes
/// - seq: u64 (8 bytes) - sequence number
/// - client_send_ts: u64 (8 bytes) - microseconds since epoch when client sends
/// - server_recv_ts: u64 (8 bytes) - microseconds since epoch when server receives (filled by server)
const TRACE_MSG_SIZE: usize = 24;

fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros() as u64
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("xoq=info".parse()?)
                .add_directive("info".parse()?),
        )
        .init();

    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        print_usage();
        return Ok(());
    }

    match args[1].as_str() {
        "server" => {
            let key_path = parse_arg(&args, "--key");
            let trace_mode = args.iter().any(|a| a == "--trace");
            run_server(key_path, trace_mode).await
        }
        "client" => {
            if args.len() < 3 {
                eprintln!("Error: client mode requires <server-id>");
                print_usage();
                return Ok(());
            }
            let server_id = &args[2];
            let count: usize = parse_arg(&args, "--count")
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_COUNT);
            let interval_ms: u64 = parse_arg(&args, "--interval")
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_INTERVAL_MS);
            let mode = match parse_arg(&args, "--mode").as_deref() {
                Some("yield") => WriteMode::Yield,
                Some("spawn") => WriteMode::Spawn,
                Some("flush") => WriteMode::Flush,
                Some("datagram") => WriteMode::Datagram,
                Some("trace") => WriteMode::Trace,
                Some("normal") | None => WriteMode::Normal,
                Some(other) => {
                    eprintln!(
                        "Unknown mode: {}. Use: normal, yield, spawn, flush, datagram, trace",
                        other
                    );
                    return Ok(());
                }
            };
            run_client(server_id, count, interval_ms, mode).await
        }
        _ => {
            print_usage();
            Ok(())
        }
    }
}

fn print_usage() {
    println!("Iroh P2P Latency Test");
    println!();
    println!("Usage:");
    println!("  iroh_latency_test server [--key <path>] [--trace]");
    println!(
        "  iroh_latency_test client <server-id> [--count <n>] [--interval <ms>] [--mode <mode>]"
    );
    println!();
    println!("Options:");
    println!("  --key <path>      Path to server identity key file");
    println!(
        "  --count <n>       Number of pings to send (default: {})",
        DEFAULT_COUNT
    );
    println!(
        "  --interval <ms>   Interval between pings in ms (default: {})",
        DEFAULT_INTERVAL_MS
    );
    println!("  --mode <mode>     Write mode: normal, yield, spawn, flush, datagram, trace (default: normal)");
    println!("  --trace           Enable detailed server-side tracing (for server)");
    println!();
    println!("Modes:");
    println!("  normal   - Standard write then read");
    println!("  yield    - Add yield_now() after write to let ConnectionDriver run");
    println!("  spawn    - Spawn write as separate task");
    println!("  flush    - Call stream flush after write");
    println!("  datagram - Use QUIC datagrams (unreliable, bypasses ACK/retransmit)");
    println!("  trace    - Detailed tracing with timestamps to identify buffering location");
    println!();
    println!("Example:");
    println!(
        "  Terminal 1: cargo run --example iroh_latency_test --features iroh -- server --trace"
    );
    println!("  Terminal 2: cargo run --example iroh_latency_test --features iroh -- client <id> --mode trace");
}

fn parse_arg(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
}

async fn run_server(key_path: Option<String>, trace_mode: bool) -> Result<()> {
    let mut builder = IrohServerBuilder::new();
    if let Some(path) = key_path {
        builder = builder.identity_path(path);
    }

    let server = builder.bind().await?;

    println!("=== Iroh Latency Test Server ===");
    println!("Server ID: {}", server.id());
    if trace_mode {
        println!("TRACE MODE: Detailed logging enabled");
    }
    println!("Waiting for connections...");
    println!();

    loop {
        if let Some(conn) = server.accept().await? {
            let remote_id = conn.remote_id();
            println!("Client connected: {}", remote_id);

            // Handle client in background
            let trace = trace_mode;
            tokio::spawn(async move {
                if let Err(e) = handle_client(conn, trace).await {
                    tracing::error!("Client {} error: {}", remote_id, e);
                }
                println!("Client {} disconnected", remote_id);
            });
        }
    }
}

async fn handle_client(conn: xoq::iroh::IrohConnection, trace_mode: bool) -> Result<()> {
    // Spawn datagram echo handler
    let conn_for_dgram = conn.connection().clone();
    let cancel = conn.cancellation_token();
    let trace = trace_mode;
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                result = conn_for_dgram.read_datagram() => {
                    match result {
                        Ok(data) => {
                            let recv_ts = now_micros();
                            if trace {
                                println!("[SERVER DGRAM] recv_ts={} len={}", recv_ts, data.len());
                            }
                            // Echo datagram back (for trace mode, include recv timestamp)
                            if trace && data.len() >= 16 {
                                // Append server recv timestamp
                                let mut response = data.to_vec();
                                if response.len() < TRACE_MSG_SIZE {
                                    response.resize(TRACE_MSG_SIZE, 0);
                                }
                                response[16..24].copy_from_slice(&recv_ts.to_le_bytes());
                                let send_ts = now_micros();
                                println!("[SERVER DGRAM] send_ts={} len={}", send_ts, response.len());
                                let _ = conn_for_dgram.send_datagram(bytes::Bytes::from(response));
                            } else {
                                let _ = conn_for_dgram.send_datagram(data);
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        }
    });

    // Handle stream echo
    let stream = conn.accept_stream().await?;
    let (mut send, mut recv) = stream.split();

    // Use larger buffer to detect coalesced reads
    let mut buf = [0u8; 1024];
    let mut pending_data = Vec::new();

    loop {
        match recv.read(&mut buf).await? {
            Some(n) if n > 0 => {
                let recv_ts = now_micros();

                if trace_mode {
                    // Log raw read - this is KEY for detecting coalescing
                    // If we see n > expected msg size, coalescing happened in network/receive path
                    println!("[SERVER STREAM] recv_ts={} raw_read_len={}", recv_ts, n);
                }

                // Append to pending data
                pending_data.extend_from_slice(&buf[..n]);

                // Process complete messages from pending_data
                // Check if this looks like trace mode (TRACE_MSG_SIZE) or normal mode (PING_SIZE)
                let msg_size = if pending_data.len() >= 16 {
                    let potential_ts = u64::from_le_bytes(pending_data[8..16].try_into().unwrap());
                    // If byte 8-16 looks like a reasonable timestamp (> 2020 in epoch micros), use trace size
                    if potential_ts > 1577836800_000_000 {
                        TRACE_MSG_SIZE
                    } else {
                        PING_SIZE
                    }
                } else {
                    PING_SIZE
                };

                // Count messages in this read to detect coalescing
                let msgs_in_read = n / msg_size;
                if trace_mode && msgs_in_read > 1 {
                    println!("[SERVER STREAM] *** COALESCING DETECTED: {} messages in single read ({} bytes)", msgs_in_read, n);
                }

                while pending_data.len() >= msg_size {
                    let msg: Vec<u8> = pending_data.drain(..msg_size).collect();

                    if trace_mode {
                        if msg_size == TRACE_MSG_SIZE && msg.len() >= 16 {
                            let seq = u64::from_le_bytes(msg[0..8].try_into().unwrap());
                            let client_send_ts = u64::from_le_bytes(msg[8..16].try_into().unwrap());
                            let server_recv_ts = recv_ts;
                            let client_to_server_us = server_recv_ts.saturating_sub(client_send_ts);
                            println!(
                                "[SERVER STREAM] seq={} client_send={} server_recv={} c2s={}us",
                                seq, client_send_ts, server_recv_ts, client_to_server_us
                            );

                            // Send response with server timestamps
                            let mut response = msg.clone();
                            response[16..24].copy_from_slice(&server_recv_ts.to_le_bytes());
                            let send_ts = now_micros();
                            println!(
                                "[SERVER STREAM] seq={} server_send={} len={}",
                                seq,
                                send_ts,
                                response.len()
                            );
                            send.write_all(&response).await?;
                        } else {
                            println!("[SERVER STREAM] echo len={}", msg.len());
                            send.write_all(&msg).await?;
                        }
                    } else {
                        // Normal echo
                        send.write_all(&msg).await?;
                    }
                }

                // If there's leftover data, log it
                if trace_mode && !pending_data.is_empty() {
                    println!(
                        "[SERVER STREAM] pending_data remaining: {} bytes",
                        pending_data.len()
                    );
                }
            }
            Some(0) | None => {
                break;
            }
            _ => {}
        }
    }

    Ok(())
}

async fn run_client(
    server_id: &str,
    count: usize,
    interval_ms: u64,
    mode: WriteMode,
) -> Result<()> {
    println!("=== Iroh Latency Test Client ===");
    println!("Connecting to: {}", server_id);
    println!(
        "Pings: {}, Interval: {}ms, Mode: {:?}",
        count, interval_ms, mode
    );
    println!();

    let conn = IrohClientBuilder::new().connect_str(server_id).await?;
    println!("Connected to {}", conn.remote_id());

    // Use datagram-specific flow if in datagram mode
    if matches!(mode, WriteMode::Datagram) {
        return run_client_datagram(&conn, count, interval_ms).await;
    }

    // Use trace mode for detailed timing analysis
    if matches!(mode, WriteMode::Trace) {
        return run_client_trace(&conn, count, interval_ms).await;
    }

    let stream = conn.open_stream().await?;
    let (send, recv) = stream.split();
    let send = Arc::new(Mutex::new(send));
    let recv = Arc::new(Mutex::new(recv));

    let interval = Duration::from_millis(interval_ms);

    let mut rtts: Vec<Duration> = Vec::with_capacity(count);
    let mut coalescing_events = 0u64;
    let coalescing_threshold = Duration::from_millis(50);

    // Warmup ping
    println!("Warming up...");
    let warmup_seq: u64 = 0;
    {
        let mut send_guard = send.lock().await;
        send_guard.write_all(&warmup_seq.to_le_bytes()).await?;
    }
    let mut buf = [0u8; PING_SIZE];
    {
        let mut recv_guard = recv.lock().await;
        recv_guard.read_exact(&mut buf).await?;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    println!("Starting latency test...");
    println!();

    let test_start = Instant::now();

    for seq in 1..=count as u64 {
        let send_time = Instant::now();

        // Send sequence number with different modes
        match mode {
            WriteMode::Normal => {
                let mut send_guard = send.lock().await;
                send_guard.write_all(&seq.to_le_bytes()).await?;
            }
            WriteMode::Yield => {
                let mut send_guard = send.lock().await;
                send_guard.write_all(&seq.to_le_bytes()).await?;
                drop(send_guard); // Release lock before yield
                tokio::task::yield_now().await;
            }
            WriteMode::Spawn => {
                let send_clone = Arc::clone(&send);
                let data = seq.to_le_bytes();
                tokio::spawn(async move {
                    let mut send_guard = send_clone.lock().await;
                    let _ = send_guard.write_all(&data).await;
                });
                // Small yield to let the spawned task run
                tokio::task::yield_now().await;
            }
            WriteMode::Flush => {
                let mut send_guard = send.lock().await;
                send_guard.write_all(&seq.to_le_bytes()).await?;
                // Note: quinn's flush is a no-op, but we try anyway
                send_guard.flush().await?;
            }
            WriteMode::Datagram | WriteMode::Trace => unreachable!("handled separately"),
        }

        // Read response with timeout
        let read_result = tokio::time::timeout(Duration::from_secs(5), async {
            let mut recv_guard = recv.lock().await;
            recv_guard.read_exact(&mut buf).await
        })
        .await;

        match read_result {
            Ok(Ok(())) => {
                let rtt = send_time.elapsed();
                let recv_seq = u64::from_le_bytes(buf);

                if recv_seq != seq {
                    tracing::warn!("Sequence mismatch: sent {}, got {}", seq, recv_seq);
                }

                if rtt > coalescing_threshold {
                    coalescing_events += 1;
                    println!(
                        "  [{}] RTT: {:.2}ms *** SPIKE (>{}ms)",
                        seq,
                        rtt.as_secs_f64() * 1000.0,
                        coalescing_threshold.as_millis()
                    );
                } else if seq % 10 == 0 || seq == count as u64 {
                    println!("  [{}] RTT: {:.2}ms", seq, rtt.as_secs_f64() * 1000.0);
                }

                rtts.push(rtt);
            }
            Ok(Err(e)) => {
                tracing::error!("Read error: {}", e);
                break;
            }
            Err(_) => {
                tracing::error!("Ping {} timed out", seq);
            }
        }

        // Wait for next interval
        let elapsed = send_time.elapsed();
        if elapsed < interval {
            tokio::time::sleep(interval - elapsed).await;
        }
    }

    let total_time = test_start.elapsed();

    // Calculate and print statistics
    println!();
    print_statistics(
        &rtts,
        count,
        coalescing_events,
        coalescing_threshold,
        total_time,
    );

    Ok(())
}

/// Trace mode: detailed timing analysis to find where buffering occurs
async fn run_client_trace(
    conn: &xoq::iroh::IrohConnection,
    count: usize,
    interval_ms: u64,
) -> Result<()> {
    println!("=== TRACE MODE: Detailed Timing Analysis ===");
    println!();
    println!(
        "Message format: {} bytes (seq + client_send_ts + server_recv_ts)",
        TRACE_MSG_SIZE
    );
    println!("Timestamps are microseconds since epoch");
    println!();

    let stream = conn.open_stream().await?;
    let (mut send, mut recv) = stream.split();

    let interval = Duration::from_millis(interval_ms);
    let mut buf = [0u8; 1024]; // Large buffer to detect coalescing

    // Stats
    let mut rtts: Vec<Duration> = Vec::with_capacity(count);
    let mut c2s_latencies: Vec<u64> = Vec::with_capacity(count);
    let mut s2c_latencies: Vec<u64> = Vec::with_capacity(count);
    let mut write_durations: Vec<Duration> = Vec::with_capacity(count);
    let mut coalescing_events = 0u64;
    let coalescing_threshold = Duration::from_millis(50);

    // Warmup
    println!("Warming up...");
    let warmup_msg = [0u8; TRACE_MSG_SIZE];
    send.write_all(&warmup_msg).await?;
    let _ = recv.read(&mut buf).await?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    println!("Starting trace test...");
    println!();
    println!("Granular timing: measuring pre-write, write_all duration, post-write");
    println!("{}", "-".repeat(80));

    let test_start = Instant::now();
    let mut last_loop_end = Instant::now();

    for seq in 1..=count as u64 {
        // Measure time since last loop iteration
        let loop_start = Instant::now();
        let inter_loop_gap = loop_start.duration_since(last_loop_end);

        // Build trace message: seq (8) + client_send_ts (8) + placeholder (8)
        let mut msg = [0u8; TRACE_MSG_SIZE];
        msg[0..8].copy_from_slice(&seq.to_le_bytes());

        // Capture timestamp just before write
        let pre_write = Instant::now();
        let client_send_ts = now_micros();
        msg[8..16].copy_from_slice(&client_send_ts.to_le_bytes());

        // ===== THE CRITICAL SECTION: write_all =====
        send.write_all(&msg).await?;
        // ============================================

        let post_write = Instant::now();
        let write_duration = post_write.duration_since(pre_write);
        write_durations.push(write_duration);

        // Log detailed client-side timing
        let write_us = write_duration.as_micros();
        let gap_us = inter_loop_gap.as_micros();

        // Flag slow writes (> 1ms indicates potential issue)
        let write_flag = if write_us > 1000 { " SLOW_WRITE" } else { "" };
        let gap_flag = if gap_us > (interval_ms as u128 * 1000 + 5000) {
            " LONG_GAP"
        } else {
            ""
        };

        println!(
            "[TX seq={}] ts={} write={}us gap={}us{}{}",
            seq, client_send_ts, write_us, gap_us, write_flag, gap_flag
        );

        // Read response with timeout
        let read_start = Instant::now();
        let read_result =
            tokio::time::timeout(Duration::from_secs(5), async { recv.read(&mut buf).await }).await;

        match read_result {
            Ok(Ok(Some(n))) => {
                let client_recv_ts = now_micros();
                let read_duration = read_start.elapsed();
                let rtt = pre_write.elapsed();

                // Log raw read size and timing
                println!(
                    "[RX seq={}] ts={} read={}us len={}",
                    seq,
                    client_recv_ts,
                    read_duration.as_micros(),
                    n
                );

                if n > TRACE_MSG_SIZE {
                    println!(
                        "  *** RX COALESCING: received {} bytes, expected {}",
                        n, TRACE_MSG_SIZE
                    );
                    coalescing_events += 1;
                }

                if n >= TRACE_MSG_SIZE {
                    let recv_seq = u64::from_le_bytes(buf[0..8].try_into().unwrap());
                    let orig_client_send = u64::from_le_bytes(buf[8..16].try_into().unwrap());
                    let server_recv_ts = u64::from_le_bytes(buf[16..24].try_into().unwrap());

                    // Calculate one-way latencies (requires synchronized clocks!)
                    let c2s_us = server_recv_ts.saturating_sub(orig_client_send);
                    let s2c_us = client_recv_ts.saturating_sub(server_recv_ts);

                    c2s_latencies.push(c2s_us);
                    s2c_latencies.push(s2c_us);

                    let rtt_ms = rtt.as_secs_f64() * 1000.0;
                    let c2s_ms = c2s_us as f64 / 1000.0;
                    let s2c_ms = s2c_us as f64 / 1000.0;

                    let spike_marker = if rtt > coalescing_threshold {
                        " *** RTT SPIKE"
                    } else {
                        ""
                    };

                    println!(
                        "  [{}] RTT={:.2}ms | c2s={:.2}ms | s2c={:.2}ms | write={:.2}ms{}",
                        recv_seq,
                        rtt_ms,
                        c2s_ms,
                        s2c_ms,
                        write_duration.as_secs_f64() * 1000.0,
                        spike_marker
                    );

                    if recv_seq != seq {
                        println!("       ^ Sequence mismatch: expected {}", seq);
                    }
                }

                if rtt > coalescing_threshold {
                    coalescing_events += 1;
                }
                rtts.push(rtt);
            }
            Ok(Ok(None)) => {
                println!("[RX] Stream closed");
                break;
            }
            Ok(Err(e)) => {
                println!("[RX] Error: {}", e);
                break;
            }
            Err(_) => {
                println!("[RX seq={}] TIMEOUT", seq);
            }
        }

        last_loop_end = Instant::now();

        // Wait for next interval
        let elapsed = pre_write.elapsed();
        if elapsed < interval {
            tokio::time::sleep(interval - elapsed).await;
        }
    }

    let total_time = test_start.elapsed();

    // Print statistics
    println!();
    println!("{}", "=".repeat(80));
    print_statistics(
        &rtts,
        count,
        coalescing_events,
        coalescing_threshold,
        total_time,
    );

    // Print write_all timing analysis
    if !write_durations.is_empty() {
        println!();
        println!("=== write_all() Timing Analysis ===");

        let mut sorted_writes: Vec<f64> = write_durations
            .iter()
            .map(|d| d.as_secs_f64() * 1000.0)
            .collect();
        sorted_writes.sort_by(|a, b| a.partial_cmp(b).unwrap());

        let write_min = sorted_writes[0];
        let write_max = sorted_writes[sorted_writes.len() - 1];
        let write_avg = sorted_writes.iter().sum::<f64>() / sorted_writes.len() as f64;
        let write_median = sorted_writes[sorted_writes.len() / 2];

        println!("write_all() duration (ms):");
        println!("  Min:    {:>8.3}", write_min);
        println!("  Max:    {:>8.3}", write_max);
        println!("  Avg:    {:>8.3}", write_avg);
        println!("  Median: {:>8.3}", write_median);

        let slow_writes = sorted_writes.iter().filter(|&&x| x > 1.0).count();
        let very_slow_writes = sorted_writes.iter().filter(|&&x| x > 10.0).count();
        println!();
        println!(
            "Slow writes (>1ms): {} ({:.1}%)",
            slow_writes,
            slow_writes as f64 / sorted_writes.len() as f64 * 100.0
        );
        println!(
            "Very slow writes (>10ms): {} ({:.1}%)",
            very_slow_writes,
            very_slow_writes as f64 / sorted_writes.len() as f64 * 100.0
        );

        if write_max > 50.0 {
            println!();
            println!(
                "*** DELAY IN write_all(): max write took {:.2}ms ***",
                write_max
            );
            println!("    This suggests buffering in quinn/iroh-quinn layer");
        }
    }

    // Print one-way latency analysis if we have data
    if !c2s_latencies.is_empty() && !s2c_latencies.is_empty() {
        println!();
        println!("=== One-Way Latency Analysis (requires synced clocks) ===");

        let c2s_avg =
            c2s_latencies.iter().sum::<u64>() as f64 / c2s_latencies.len() as f64 / 1000.0;
        let s2c_avg =
            s2c_latencies.iter().sum::<u64>() as f64 / s2c_latencies.len() as f64 / 1000.0;

        let c2s_max = *c2s_latencies.iter().max().unwrap() as f64 / 1000.0;
        let s2c_max = *s2c_latencies.iter().max().unwrap() as f64 / 1000.0;

        println!(
            "Client -> Server: avg={:.2}ms, max={:.2}ms",
            c2s_avg, c2s_max
        );
        println!(
            "Server -> Client: avg={:.2}ms, max={:.2}ms",
            s2c_avg, s2c_max
        );

        // Find where spikes occur
        let c2s_spikes = c2s_latencies.iter().filter(|&&x| x > 50_000).count();
        let s2c_spikes = s2c_latencies.iter().filter(|&&x| x > 50_000).count();
        println!();
        println!("Spikes (>50ms):");
        println!("  Client -> Server: {}", c2s_spikes);
        println!("  Server -> Client: {}", s2c_spikes);

        // Correlate write duration with RTT spikes
        let spike_indices: Vec<usize> = rtts
            .iter()
            .enumerate()
            .filter(|(_, rtt)| **rtt > coalescing_threshold)
            .map(|(i, _)| i)
            .collect();

        if !spike_indices.is_empty() && !write_durations.is_empty() {
            println!();
            println!("=== Spike Correlation Analysis ===");
            let spike_write_times: Vec<f64> = spike_indices
                .iter()
                .filter_map(|&i| write_durations.get(i))
                .map(|d| d.as_secs_f64() * 1000.0)
                .collect();

            if !spike_write_times.is_empty() {
                let spike_write_avg =
                    spike_write_times.iter().sum::<f64>() / spike_write_times.len() as f64;
                let spike_write_max = spike_write_times.iter().cloned().fold(0.0, f64::max);

                println!("write_all() duration during RTT spikes:");
                println!(
                    "  Avg: {:.3}ms, Max: {:.3}ms",
                    spike_write_avg, spike_write_max
                );

                if spike_write_avg > 5.0 {
                    println!();
                    println!("*** CORRELATION FOUND: RTT spikes coincide with slow writes ***");
                    println!("    Buffering is happening INSIDE write_all() / quinn layer");
                } else {
                    println!();
                    println!("*** NO CORRELATION: write_all() is fast during spikes ***");
                    println!("    Buffering is happening AFTER write_all() (kernel/network)");
                }
            }
        }

        if c2s_spikes > s2c_spikes * 2 {
            println!();
            println!("*** BUFFERING likely on CLIENT SEND or NETWORK path ***");
        } else if s2c_spikes > c2s_spikes * 2 {
            println!();
            println!("*** BUFFERING likely on SERVER SEND or NETWORK path ***");
        }
    }

    Ok(())
}

async fn run_client_datagram(
    conn: &xoq::iroh::IrohConnection,
    count: usize,
    interval_ms: u64,
) -> Result<()> {
    let interval = Duration::from_millis(interval_ms);
    let inner_conn = conn.connection();

    let mut rtts: Vec<Duration> = Vec::with_capacity(count);
    let mut coalescing_events = 0u64;
    let coalescing_threshold = Duration::from_millis(50);

    // Check datagram support
    if inner_conn.max_datagram_size().is_none() {
        anyhow::bail!("Datagrams not supported by peer");
    }

    // Warmup
    println!("Warming up (datagram mode)...");
    let warmup_data = bytes::Bytes::from(0u64.to_le_bytes().to_vec());
    inner_conn.send_datagram(warmup_data)?;
    let _ = tokio::time::timeout(Duration::from_secs(1), inner_conn.read_datagram()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    println!("Starting datagram latency test...");
    println!();

    let test_start = Instant::now();

    for seq in 1..=count as u64 {
        let send_time = Instant::now();

        // Send datagram
        let data = bytes::Bytes::from(seq.to_le_bytes().to_vec());
        inner_conn.send_datagram(data)?;

        // Read response with timeout
        let read_result =
            tokio::time::timeout(Duration::from_secs(5), inner_conn.read_datagram()).await;

        match read_result {
            Ok(Ok(data)) => {
                let rtt = send_time.elapsed();

                if data.len() == PING_SIZE {
                    let recv_seq = u64::from_le_bytes(data[..8].try_into().unwrap());
                    if recv_seq != seq {
                        tracing::warn!("Sequence mismatch: sent {}, got {}", seq, recv_seq);
                    }
                }

                if rtt > coalescing_threshold {
                    coalescing_events += 1;
                    println!(
                        "  [{}] RTT: {:.2}ms *** SPIKE (>{}ms)",
                        seq,
                        rtt.as_secs_f64() * 1000.0,
                        coalescing_threshold.as_millis()
                    );
                } else if seq % 10 == 0 || seq == count as u64 {
                    println!("  [{}] RTT: {:.2}ms", seq, rtt.as_secs_f64() * 1000.0);
                }

                rtts.push(rtt);
            }
            Ok(Err(e)) => {
                tracing::error!("Datagram read error: {}", e);
                break;
            }
            Err(_) => {
                tracing::error!("Datagram {} timed out", seq);
            }
        }

        // Wait for next interval
        let elapsed = send_time.elapsed();
        if elapsed < interval {
            tokio::time::sleep(interval - elapsed).await;
        }
    }

    let total_time = test_start.elapsed();

    println!();
    print_statistics(
        &rtts,
        count,
        coalescing_events,
        coalescing_threshold,
        total_time,
    );

    Ok(())
}

fn print_statistics(
    rtts: &[Duration],
    expected_count: usize,
    coalescing_events: u64,
    threshold: Duration,
    total_time: Duration,
) {
    if rtts.is_empty() {
        println!("No successful pings recorded.");
        return;
    }

    let mut sorted: Vec<f64> = rtts.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let min = sorted[0];
    let max = sorted[sorted.len() - 1];
    let sum: f64 = sorted.iter().sum();
    let avg = sum / sorted.len() as f64;
    let median = if sorted.len() % 2 == 0 {
        (sorted[sorted.len() / 2 - 1] + sorted[sorted.len() / 2]) / 2.0
    } else {
        sorted[sorted.len() / 2]
    };

    // Percentiles
    let p95_idx = (sorted.len() as f64 * 0.95).ceil() as usize - 1;
    let p99_idx = (sorted.len() as f64 * 0.99).ceil() as usize - 1;
    let p95 = sorted[p95_idx.min(sorted.len() - 1)];
    let p99 = sorted[p99_idx.min(sorted.len() - 1)];

    // Standard deviation
    let variance: f64 = sorted.iter().map(|x| (x - avg).powi(2)).sum::<f64>() / sorted.len() as f64;
    let std_dev = variance.sqrt();

    // RFC 3550 jitter (exponential moving average of |delta - mean_delta|)
    let mut jitter = 0.0f64;
    for i in 1..sorted.len() {
        let delta = (sorted[i] - sorted[i - 1]).abs();
        jitter += (delta - jitter) / 16.0;
    }

    // Latency histogram
    let buckets = [1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0];
    let mut histogram: Vec<usize> = vec![0; buckets.len() + 1];
    for &rtt in &sorted {
        let mut placed = false;
        for (i, &limit) in buckets.iter().enumerate() {
            if rtt <= limit {
                histogram[i] += 1;
                placed = true;
                break;
            }
        }
        if !placed {
            histogram[buckets.len()] += 1;
        }
    }

    println!("=== Latency Statistics ===");
    println!();
    println!(
        "Packets: {} sent, {} received ({:.1}% success)",
        expected_count,
        rtts.len(),
        rtts.len() as f64 / expected_count as f64 * 100.0
    );
    println!("Total time: {:.2}s", total_time.as_secs_f64());
    println!();
    println!("Round-trip latency (ms):");
    println!("  Min:    {:>8.2}", min);
    println!("  Max:    {:>8.2}", max);
    println!("  Avg:    {:>8.2}", avg);
    println!("  Median: {:>8.2}", median);
    println!("  P95:    {:>8.2}", p95);
    println!("  P99:    {:>8.2}", p99);
    println!();
    println!("Jitter:");
    println!("  Std Dev:     {:>8.2}ms", std_dev);
    println!("  RFC 3550:    {:>8.2}ms", jitter);
    println!();
    println!(
        "Coalescing detection (spikes >{}ms):",
        threshold.as_millis()
    );
    println!(
        "  Count: {} ({:.1}% of packets)",
        coalescing_events,
        coalescing_events as f64 / rtts.len() as f64 * 100.0
    );
    println!();
    println!("Latency histogram:");
    println!(
        "  <= 1ms:   {:>5} ({:>5.1}%)",
        histogram[0],
        histogram[0] as f64 / rtts.len() as f64 * 100.0
    );
    println!(
        "  <= 2ms:   {:>5} ({:>5.1}%)",
        histogram[1],
        histogram[1] as f64 / rtts.len() as f64 * 100.0
    );
    println!(
        "  <= 5ms:   {:>5} ({:>5.1}%)",
        histogram[2],
        histogram[2] as f64 / rtts.len() as f64 * 100.0
    );
    println!(
        "  <= 10ms:  {:>5} ({:>5.1}%)",
        histogram[3],
        histogram[3] as f64 / rtts.len() as f64 * 100.0
    );
    println!(
        "  <= 20ms:  {:>5} ({:>5.1}%)",
        histogram[4],
        histogram[4] as f64 / rtts.len() as f64 * 100.0
    );
    println!(
        "  <= 50ms:  {:>5} ({:>5.1}%)",
        histogram[5],
        histogram[5] as f64 / rtts.len() as f64 * 100.0
    );
    println!(
        "  <= 100ms: {:>5} ({:>5.1}%)",
        histogram[6],
        histogram[6] as f64 / rtts.len() as f64 * 100.0
    );
    println!(
        "  <= 200ms: {:>5} ({:>5.1}%)",
        histogram[7],
        histogram[7] as f64 / rtts.len() as f64 * 100.0
    );
    println!(
        "  <= 500ms: {:>5} ({:>5.1}%)",
        histogram[8],
        histogram[8] as f64 / rtts.len() as f64 * 100.0
    );
    println!(
        "  > 500ms:  {:>5} ({:>5.1}%)",
        histogram[9],
        histogram[9] as f64 / rtts.len() as f64 * 100.0
    );
}
