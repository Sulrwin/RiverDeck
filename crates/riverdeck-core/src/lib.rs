//! RiverDeck backend core.
//!
//! This crate is extracted from the former `src-tauri` backend to allow multiple frontends
//! (egui main UI, embedded property inspector webviews, etc.) without a hard dependency on Tauri.

pub mod application_watcher;
pub mod elgato;
pub mod events;
pub mod plugins;
pub mod shared;
pub mod store;
pub mod zip_extract;

pub mod api;

pub mod ui;
pub mod webview;
