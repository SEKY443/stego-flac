//! Payload coding: compression, authenticated encryption, and erasure coding.
//!
//! These sit above the physical layer and know nothing about audio.

pub mod compress;
pub mod crypto;
pub mod fec;

pub use compress::DEFAULT_LEVEL as DEFAULT_COMPRESSION_LEVEL;
pub use crypto::{DerivedKey, KdfParams};
pub use fec::FecParams;
