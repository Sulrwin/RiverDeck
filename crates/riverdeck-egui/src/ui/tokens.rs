#[derive(Debug, Clone, Copy)]
pub struct UiTokens {
    pub radius_window: u8,
    pub radius_card: u8,
    pub radius_control: u8,

    pub stroke_width: f32,
    pub stroke_width_active: f32,

    pub item_spacing_x: f32,
    pub item_spacing_y: f32,
    pub button_pad_x: f32,
    pub button_pad_y: f32,
    pub window_margin: i8,
}

impl UiTokens {
    pub fn modern_neutral() -> Self {
        Self {
            radius_window: 12,
            radius_card: 12,
            radius_control: 10,
            stroke_width: 1.0,
            stroke_width_active: 2.0,
            item_spacing_x: 12.0,
            item_spacing_y: 12.0,
            button_pad_x: 11.0,
            button_pad_y: 9.0,
            window_margin: 14,
        }
    }
}
