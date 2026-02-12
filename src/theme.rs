use ratatui::style::{Color, Modifier, Style};

// Base colors
pub const BG: Color = Color::Rgb(18, 18, 24);
pub const BG_SELECTED: Color = Color::Rgb(40, 44, 62);
pub const BG_HEADER: Color = Color::Rgb(24, 24, 32);
pub const FG_DIM: Color = Color::Rgb(90, 90, 110);
pub const FG_TEXT: Color = Color::Rgb(188, 188, 200);

// Mail list colors
pub const SENDER_COLOR: Color = Color::Rgb(130, 170, 255);
pub const SUBJECT_COLOR: Color = Color::Rgb(220, 220, 235);
pub const UNREAD_MARKER: Color = Color::Rgb(80, 200, 120);
pub const TIME_RELATIVE: Color = Color::Rgb(200, 160, 80);
pub const TIME_EXACT: Color = Color::Rgb(80, 80, 100);
pub const PREVIEW_COLOR: Color = Color::Rgb(100, 100, 120);

// Detail view colors
pub const DETAIL_HEADER_LABEL: Color = Color::Rgb(70, 70, 90);
pub const DETAIL_HEADER_VALUE: Color = Color::Rgb(160, 160, 180);
pub const DETAIL_FROM: Color = Color::Rgb(130, 170, 255);
pub const DETAIL_SUBJECT: Color = Color::Rgb(240, 240, 255);
pub const DETAIL_BODY: Color = Color::Rgb(200, 200, 215);

// Status bar
pub const STATUS_BG: Color = Color::Rgb(30, 32, 44);
pub const STATUS_FG: Color = Color::Rgb(140, 140, 160);
pub const STATUS_KEY: Color = Color::Rgb(200, 160, 80);

// Styles
pub fn style_selected() -> Style {
    Style::default().bg(BG_SELECTED)
}

pub fn style_unread_marker() -> Style {
    Style::default().fg(UNREAD_MARKER).add_modifier(Modifier::BOLD)
}

pub fn style_sender() -> Style {
    Style::default().fg(SENDER_COLOR)
}

pub fn style_sender_selected() -> Style {
    Style::default().fg(SENDER_COLOR).bg(BG_SELECTED)
}

pub fn style_subject() -> Style {
    Style::default().fg(SUBJECT_COLOR)
}

pub fn style_subject_selected() -> Style {
    Style::default().fg(SUBJECT_COLOR).bg(BG_SELECTED)
}

pub fn style_time_relative() -> Style {
    Style::default().fg(TIME_RELATIVE)
}

pub fn style_time_exact() -> Style {
    Style::default().fg(TIME_EXACT)
}

pub fn style_preview() -> Style {
    Style::default().fg(PREVIEW_COLOR)
}

pub fn style_detail_header_label() -> Style {
    Style::default().fg(DETAIL_HEADER_LABEL)
}

pub fn style_detail_header_value() -> Style {
    Style::default().fg(DETAIL_HEADER_VALUE)
}

pub fn style_detail_from() -> Style {
    Style::default().fg(DETAIL_FROM).add_modifier(Modifier::BOLD)
}

pub fn style_detail_subject() -> Style {
    Style::default()
        .fg(DETAIL_SUBJECT)
        .add_modifier(Modifier::BOLD)
}

pub fn style_detail_body() -> Style {
    Style::default().fg(DETAIL_BODY)
}

pub fn style_status_bar() -> Style {
    Style::default().fg(STATUS_FG).bg(STATUS_BG)
}

pub fn style_status_key() -> Style {
    Style::default().fg(STATUS_KEY).bg(STATUS_BG)
}
