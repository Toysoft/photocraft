//! Android entry point and platform services for PhotoCraft.
//!
//! The editor itself stays in the shared Rust crates. This application package only owns Android
//! lifecycle, Storage Access Framework document selection, and files private to the app.

#![deny(clippy::expect_used, clippy::panic, clippy::todo, clippy::unimplemented, clippy::unreachable, clippy::unwrap_used)]

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
mod tablet_gestures;

#[cfg(target_os = "android")]
mod android;
