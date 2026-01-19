//! Python bindings for xoq
//!
//! Provides Python access to MoQ and iroh P2P communication.
//! All functions are blocking (synchronous).

// PyO3 macros generate code that triggers this lint incorrectly
#![allow(clippy::useless_conversion)]

use pyo3::prelude::*;

// Use external crate with explicit path to avoid shadowing by our module name
use ::xoq as xoq_lib;

// Global tokio runtime for blocking calls
fn runtime() -> &'static tokio::runtime::Runtime {
    use std::sync::OnceLock;
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| tokio::runtime::Runtime::new().expect("Failed to create tokio runtime"))
}

// ============================================================================
// Module declarations
// ============================================================================

mod moq;

#[cfg(feature = "iroh")]
mod iroh;

#[cfg(all(feature = "serial", feature = "iroh"))]
mod serial_server;

#[cfg(all(feature = "serial", feature = "iroh"))]
mod serial;

#[cfg(all(feature = "camera", feature = "iroh"))]
mod camera_server;

#[cfg(all(feature = "camera", feature = "iroh"))]
mod opencv;

// ============================================================================
// Python module registration
// ============================================================================

#[pymodule]
fn xoq(m: &Bound<'_, PyModule>) -> PyResult<()> {
    use pyo3::types::PyModule;
    let py = m.py();

    // MoQ classes
    m.add_class::<moq::MoqConnection>()?;
    m.add_class::<moq::MoqPublisher>()?;
    m.add_class::<moq::MoqSubscriber>()?;
    m.add_class::<moq::MoqTrackWriter>()?;
    m.add_class::<moq::MoqTrackReader>()?;

    // Iroh classes (when feature enabled)
    #[cfg(feature = "iroh")]
    {
        m.add_class::<iroh::IrohServer>()?;
        m.add_class::<iroh::IrohConnection>()?;
        m.add_class::<iroh::IrohStream>()?;
    }

    // Serial classes (when both iroh and serial features enabled)
    #[cfg(all(feature = "serial", feature = "iroh"))]
    {
        m.add_class::<serial_server::Server>()?;
        m.add_class::<serial::Serial>()?;
    }

    // Camera classes (when both camera and iroh features enabled)
    #[cfg(all(feature = "camera", feature = "iroh"))]
    {
        m.add_class::<camera_server::CameraServer>()?;
        m.add_class::<opencv::VideoCapture>()?;
    }

    // Create xoq.serial submodule (pyserial-compatible interface)
    #[cfg(all(feature = "serial", feature = "iroh"))]
    {
        let serial_mod = PyModule::new_bound(py, "serial")?;
        serial_mod.add_class::<serial::Serial>()?;

        m.add_submodule(&serial_mod)?;
    }

    Ok(())
}
