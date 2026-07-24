//! Core library for the Cirrusync Cloudflare Dynamic DNS client.

pub mod cloudflare;
pub mod config;
pub mod daemon;
pub mod error;
pub mod public_ip;
pub mod updater;

/// User agent sent to public-address providers and Cloudflare.
pub const USER_AGENT: &str = concat!("cirrusync/", env!("CARGO_PKG_VERSION"));
