#![forbid(unsafe_code)]
//! enzremote signaling server library.
//!
//! Provides WebSocket-based session control and WebRTC signaling for
//! LAN clients. The server manages session lifecycle, SDP/ICE relay,
//! and mDNS service discovery.

pub mod approval;
pub mod clipboard;
pub mod audit;
pub mod control;
pub mod desktop;
pub mod desktop_input;
mod desktop_lease;
mod desktop_mirror;
pub mod discovery;
pub mod error;
pub mod identity;
pub mod launcher;
pub mod media;
pub mod pairing;
pub mod phone_notify;
pub mod protocol;
pub mod host_settings;
pub mod second_display;
pub mod sms;
pub mod server;
pub mod sessions;
pub mod share;
pub mod shared_desktop;
#[cfg(feature = "tls")]
pub mod tls;
pub mod uplink;
pub mod webrtc_bridge;

mod session_access;
