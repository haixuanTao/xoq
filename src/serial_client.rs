//! Sync serial client for Python bindings.
//!
//! This module provides a blocking API for remote serial ports,
//! managing its own tokio runtime internally.

use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use crate::iroh::IrohClientBuilder;

/// A synchronous client for remote serial ports.
///
/// This client manages its own tokio runtime internally,
/// providing a simple blocking API.
pub struct SyncSerialClient {
    recv: Arc<Mutex<iroh::endpoint::RecvStream>>,
    write_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    runtime: tokio::runtime::Runtime,
    _conn: crate::iroh::IrohConnection,
}

impl SyncSerialClient {
    /// Connect to a remote serial port server.
    pub fn connect(server_id: &str) -> Result<Self> {
        let runtime = tokio::runtime::Runtime::new()?;

        let (write_tx, recv, conn) = runtime.block_on(async {
            let conn = IrohClientBuilder::new().connect_str(server_id).await?;
            let stream = conn.open_stream().await?;
            let (send, recv) = stream.split();

            // Spawn background writer task so writes don't go through block_on
            let (write_tx, mut write_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
            tokio::spawn(async move {
                let mut send = send;
                while let Some(data) = write_rx.recv().await {
                    if send.write_all(&data).await.is_err() {
                        break;
                    }
                }
            });

            Ok::<_, anyhow::Error>((write_tx, recv, conn))
        })?;

        Ok(Self {
            recv: Arc::new(Mutex::new(recv)),
            write_tx,
            runtime,
            _conn: conn,
        })
    }

    /// Write data to the remote serial port.
    pub fn write(&self, data: &[u8]) -> Result<()> {
        self.write_tx
            .blocking_send(data.to_vec())
            .map_err(|_| anyhow::anyhow!("write channel closed"))
    }

    /// Read data from the remote serial port.
    pub fn read(&self, buf: &mut [u8]) -> Result<Option<usize>> {
        self.runtime.block_on(async {
            let mut recv = self.recv.lock().await;
            Ok(recv.read(buf).await?)
        })
    }

    /// Read with timeout.
    pub fn read_timeout(&self, buf: &mut [u8], timeout: Duration) -> Result<Option<usize>> {
        self.runtime.block_on(async {
            match tokio::time::timeout(timeout, async {
                let mut recv = self.recv.lock().await;
                recv.read(buf).await
            })
            .await
            {
                Ok(Ok(n)) => Ok(n),
                Ok(Err(e)) => Err(e.into()),
                Err(_) => Ok(None), // Timeout
            }
        })
    }
}
