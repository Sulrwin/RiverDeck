//! RiverDeck backend core.
//!
//! This crate is extracted from the former Tauri backend to allow multiple frontends
//! (egui main UI, embedded property inspector webviews, etc.) without a hard dependency on Tauri.

pub mod animation;
pub mod application_watcher;
pub mod elgato;
pub mod events;
pub mod lifecycle;
pub mod plugins;
pub mod runtime_processes;
pub mod shared;
pub mod store;
pub mod zip_extract;

pub mod api;

pub mod ui;
pub mod webview;
