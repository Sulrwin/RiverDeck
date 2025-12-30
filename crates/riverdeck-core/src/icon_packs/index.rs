use serde::{Deserialize, Serialize};

/// `icons.json` for a Stream Deck icon pack.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct IconPackIndex {
    pub icons: Vec<IconEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct IconEntry {
    /// Path to the icon file.
    ///
    /// In official packs this is typically relative to the pack root (e.g. `icons/foo.png`).
    /// RiverDeck also supports `riverdeck/...` virtual paths for its bundled pack.
    pub path: String,
    pub name: String,
    pub tags: Vec<String>,
}
