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

// Thread indicators
pub const THREAD_INDICATOR: Color = Color::Rgb(100, 140, 200);
pub const THREAD_BRANCH: Color = Color::Rgb(60, 60, 80);

// Attachments
pub const ATTACHMENT_COLOR: Color = Color::Rgb(180, 140, 60);
pub const ATTACHMENT_SELECTED: Color = Color::Rgb(255, 200, 80);
pub const ATTACHMENT_SELECTED_BG: Color = Color::Rgb(50, 45, 30);

// Links
pub const LINK_COLOR: Color = Color::Rgb(100, 180, 240);
pub const LINK_SELECTED: Color = Color::Rgb(140, 220, 255);
pub const LINK_SELECTED_BG: Color = Color::Rgb(30, 50, 80);

// Mode indicator
pub const MODE_INDICATOR: Color = Color::Rgb(180, 140, 60);

// Selection highlight
pub const SELECTION_BG: Color = Color::Rgb(50, 80, 140);

// Status bar
pub const STATUS_BG: Color = Color::Rgb(30, 32, 44);
pub const STATUS_FG: Color = Color::Rgb(140, 140, 160);
pub const STATUS_KEY: Color = Color::Rgb(200, 160, 80);

// Sent/Draft in-flight state badges (list rows)
pub const STATUS_ERROR: Color = Color::Rgb(240, 100, 100);
pub const STATUS_PENDING: Color = Color::Rgb(200, 160, 80);
pub const STATUS_SUCCESS: Color = Color::Rgb(80, 200, 120);

// Help overlay
pub const HELP_BG: Color = Color::Rgb(28, 30, 42);
pub const HELP_BORDER: Color = Color::Rgb(100, 140, 200);
pub const HELP_KEY: Color = Color::Rgb(200, 160, 80);
pub const HELP_DESC: Color = Color::Rgb(180, 180, 195);
pub const HELP_TITLE: Color = Color::Rgb(130, 170, 255);

// Compose view
pub const COMPOSE_BORDER: Color = Color::Rgb(100, 140, 200);
pub const COMPOSE_FIELD_ACTIVE: Color = Color::Rgb(80, 200, 120);
pub const COMPOSE_CURSOR: Color = Color::Rgb(200, 200, 220);
pub const COMPOSE_QUOTE: Color = Color::Rgb(100, 140, 180);
pub const COMPOSE_DROPDOWN_BG: Color = Color::Rgb(35, 38, 52);

// Account colors for global inbox
pub const ACCOUNT_COLORS: &[Color] = &[
    Color::Rgb(130, 170, 255), // Blue
    Color::Rgb(200, 130, 255), // Purple
    Color::Rgb(80, 200, 120),  // Green
    Color::Rgb(255, 180, 80),  // Orange
    Color::Rgb(255, 120, 120), // Red
    Color::Rgb(100, 220, 220), // Cyan
    Color::Rgb(255, 160, 200), // Pink
    Color::Rgb(200, 200, 100), // Yellow
];

// Styles
pub fn style_selected() -> Style {
    Style::default().bg(BG_SELECTED)
}

pub fn style_unread_marker() -> Style {
    Style::default()
        .fg(UNREAD_MARKER)
        .add_modifier(Modifier::BOLD)
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
    Style::default()
        .fg(DETAIL_FROM)
        .add_modifier(Modifier::BOLD)
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

pub fn style_link() -> Style {
    Style::default()
        .fg(LINK_COLOR)
        .add_modifier(Modifier::UNDERLINED)
}

pub fn style_link_selected() -> Style {
    Style::default()
        .fg(LINK_SELECTED)
        .bg(LINK_SELECTED_BG)
        .add_modifier(Modifier::UNDERLINED | Modifier::BOLD)
}

/// Style for a Sent/Draft list row's in-flight/error status badge, chosen
/// by simple keyword sniffing of the label text (kept here rather than a
/// separate enum so `db::local_message_status_label` can stay a plain
/// string builder with no UI dependency).
pub fn style_status_label(label: &str) -> Style {
    let lower = label.to_lowercase();
    if lower.contains("failed") {
        Style::default().fg(STATUS_ERROR)
    } else if lower.contains("uploaded") {
        Style::default().fg(STATUS_SUCCESS)
    } else {
        Style::default().fg(STATUS_PENDING)
    }
}
