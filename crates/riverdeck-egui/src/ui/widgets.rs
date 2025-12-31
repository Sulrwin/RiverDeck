use egui::{Response, RichText, Ui, Vec2};

pub fn icon_button(ui: &mut Ui, label: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(label).size(13.0))
            .min_size(Vec2::new(24.0, 20.0))
            .frame(false),
    )
}

pub fn section_heading(ui: &mut Ui, label: &str) {
    ui.label(RichText::new(label).strong());
}

pub fn search_field(ui: &mut Ui, value: &mut String, hint: &str) -> Response {
    ui.add(
        egui::TextEdit::singleline(value)
            .hint_text(hint)
            .desired_width(f32::INFINITY)
            .margin(Vec2::new(10.0, 8.0)),
    )
}

pub fn paint_list_row(
    ui: &egui::Ui,
    rect: egui::Rect,
    radius: egui::CornerRadius,
    fill: egui::Color32,
    stroke: egui::Stroke,
) {
    ui.painter()
        .rect(rect, radius, fill, stroke, egui::StrokeKind::Inside);
}
