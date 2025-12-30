use elgato_streamdeck::info::Kind;

/// Return the native keypad button image size (in pixels) for a RiverDeck `DeviceInfo.r#type`.
///
/// RiverDeck uses a small numeric `type` in `DeviceInfo` (mirroring the Stream Deck SDK).
/// The mapping here follows `elgato.rs:init()` which assigns those values.
pub fn key_image_size_for_device_type(device_type: u8) -> (u32, u32) {
    let kind = match device_type {
        // Original / MK2 family.
        0 => Kind::Mk2,
        // Mini family.
        1 => Kind::MiniMk2,
        // XL family.
        2 => Kind::XlV2,
        // Pedal (no keypad, but keep a sensible default).
        5 => Kind::Pedal,
        // Plus (has a keypad).
        7 => Kind::Plus,
        // Neo.
        9 => Kind::Neo,
        // Unknown: default to the most common 72x72 keys.
        _ => Kind::Mk2,
    };

    let (w, h) = kind.key_image_format().size;
    (w as u32, h as u32)
}
