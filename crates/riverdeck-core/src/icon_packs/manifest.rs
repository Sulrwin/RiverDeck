use serde::{Deserialize, Serialize};

/// `manifest.json` for a Stream Deck icon pack (`*.sdIconPack`).
///
/// The exact schema varies a bit across pack versions; we only parse the fields we need.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct IconPackManifest {
    pub name: String,
    pub author: String,
    pub version: String,
}
