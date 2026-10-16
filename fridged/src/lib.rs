//! `fridged`: keeps a refrigerator inside a temperature band and publishes
//! telemetry to Cloud Firestore.
//!
//! The logic lives in this library so it can be tested on the development
//! machine; `main.rs` only wires it to the hardware.

pub mod channel;
pub mod cli;
pub mod clock;
pub mod config;
pub mod control;
pub mod daemon;
pub mod error;
pub mod firestore;
pub mod healthcheck;
pub mod logging;
pub mod relay;
pub mod sample;
pub mod sensor;
pub mod settings;
pub mod signals;
pub mod sim;
pub mod spool;
pub mod systemd;
pub mod timestamp;
pub mod uploader;

pub use error::{Error, Result};
