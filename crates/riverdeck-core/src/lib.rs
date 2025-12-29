//! RiverDeck backend core.
//!
//! This crate is extracted from the former Tauri backend to allow multiple frontends
//! (egui main UI, embedded property inspector webviews, etc.) without a hard dependency on Tauri.

pub mod animation;
pub mod application_watcher;
pub mod elgato;

// Re-export SVG conversion function for use in UI
pub use elgato::convert_svg_to_image;
pub mod events;
pub mod icon_packs;
pub mod lifecycle;
pub mod marketplace;
pub mod openaction_marketplace;
pub mod plugins;
pub mod render;
pub mod runtime_processes;
pub mod shared;
pub mod store;
pub mod zip_extract;

pub mod api;

pub mod ui;
pub mod webview;
