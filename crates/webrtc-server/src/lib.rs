#![forbid(unsafe_code)]
//! enzremote-webrtc-server library: IPC protocol types, input bridge,
//! and pipeline construction for the per-session WebRTC process.
//!
//! The `ipc` module defines the message types exchanged between the
//! signaling server and the WebRTC pipeline process over a Unix socket.
//!
//! The `input_bridge` module manages the C EIS input worker process.
//!
//! The `wayland_input` module provides a fallback input path using
//! `ydotool` and `wtype` for Wayland-native input injection.
//!
//! The `pipeline` module contains the GStreamer WebRTC pipeline.

pub mod congestion;
mod content_viewport;
pub mod gamepad;
pub mod impairment;
pub mod input_bridge;
pub mod ipc;
pub mod pipeline;
pub mod playout;
pub mod portal;
pub mod wayland_input;
