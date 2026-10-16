//! Cloud Firestore over its REST API: authentication with a service account,
//! the typed document format, and the retry policy.
//!
//! REST rather than gRPC: `tonic` and its dependencies are too heavy to build
//! for ARMv6. Blocking `ureq` calls, run only from the uploader thread, never
//! from the control thread.

pub mod auth;
pub mod client;
pub mod value;
