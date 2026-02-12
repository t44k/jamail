use crate::db::MailDb;
use crate::mail::{open_in_browser, relative_time, Email, EmailContent};
use crate::theme;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap},
    Frame,
};

const LIST_PEEK_WIDTH: u16 = 12;

pub enum ViewMode {
    List,
    Detail,
    Search,
}

pub struct App {
    pub emails: Vec<Email>,
    pub selected: usize,
    pub view: ViewMode,
    pub detail: Option<EmailContent>,
    pub detail_scroll: u16,
    pub detail_from_search: bool,
    pub list_scroll_offset: usize,
    pub status_msg: String,
    pub should_quit: bool,
    pub search_query: String,
    pub search_results: Vec<Email>,
    pub search_selected: usize,
    pub search_scroll_offset: usize,
}

impl App {
    pub fn new(emails: Vec<Email>) -> Self {
        Self {
            emails,
            selected: 0,
            view: ViewMode::List,
            detail: None,
            detail_scroll: 0,
            detail_from_search: false,
            list_scroll_offset: 0,
            status_msg: String::new(),
            should_quit: false,
            search_query: String::new(),
            search_results: Vec::new(),
            search_selected: 0,
            search_scroll_offset: 0,
        }
    }

    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.emails.len() {
            self.selected += 1;
        }
    }

    pub fn open_detail(&mut self, db: &MailDb) {
        self.detail_from_search = matches!(self.view, ViewMode::Search);
        let (uid, list_idx) = match self.view {
            ViewMode::Search => {
                if let Some(email) = self.search_results.get(self.search_selected) {
                    // Find the index in the main list for marking read
                    let idx = self.emails.iter().position(|e| e.uid == email.uid);
                    (email.uid, idx)
                } else {
                    return;
                }
            }
            _ => {
                if let Some(email) = self.emails.get(self.selected) {
                    (email.uid, Some(self.selected))
                } else {
                    return;
                }
            }
        };

        match db.get_email_content(uid) {
            Ok(Some(content)) => {
                self.detail = Some(content);
                self.detail_scroll = 0;
                self.view = ViewMode::Detail;
                // Mark as read in DB and local list
                let _ = db.mark_read(uid);
                if let Some(idx) = list_idx {
                    if let Some(e) = self.emails.get_mut(idx) {
                        e.is_unread = false;
                    }
                }
            }
            Ok(None) => {
                self.status_msg = "Email not found in cache".to_string();
            }
            Err(e) => {
                self.status_msg = format!("Error: {}", e);
            }
        }
    }

    pub fn enter_search(&mut self) {
        self.search_query.clear();
        self.search_results.clear();
        self.search_selected = 0;
        self.search_scroll_offset = 0;
        self.view = ViewMode::Search;
    }

    pub fn search_input(&mut self, ch: char) {
        self.search_query.push(ch);
        self.search_results.clear();
    }

    pub fn search_backspace(&mut self) {
        self.search_query.pop();
        self.search_results.clear();
    }

    pub fn execute_search(&mut self, db: &MailDb) {
        if self.search_query.is_empty() {
            return;
        }
        match db.search_emails(&self.search_query) {
            Ok(results) => {
                self.search_results = results;
                self.search_selected = 0;
                self.search_scroll_offset = 0;
                self.status_msg = format!("{} results", self.search_results.len());
            }
            Err(e) => {
                self.status_msg = format!("Search error: {}", e);
                self.search_results.clear();
            }
        }
    }

    pub fn exit_search(&mut self) {
        self.view = ViewMode::List;
        self.search_query.clear();
        self.search_results.clear();
    }

    pub fn search_move_up(&mut self) {
        if self.search_selected > 0 {
            self.search_selected -= 1;
        }
    }

    pub fn search_move_down(&mut self) {
        if self.search_selected + 1 < self.search_results.len() {
            self.search_selected += 1;
        }
    }

    pub fn refresh_emails(&mut self, db: &MailDb) {
        let selected_uid = self.emails.get(self.selected).map(|e| e.uid);
        if let Ok(emails) = db.get_email_list() {
            self.emails = emails;
            if let Some(uid) = selected_uid {
                if let Some(idx) = self.emails.iter().position(|e| e.uid == uid) {
                    self.selected = idx;
                }
            }
            if self.selected >= self.emails.len() && !self.emails.is_empty() {
                self.selected = self.emails.len() - 1;
            }
        }
    }

    pub fn close_detail(&mut self) {
        self.view = if self.detail_from_search {
            ViewMode::Search
        } else {
            ViewMode::List
        };
        self.detail = None;
        self.detail_scroll = 0;
        self.detail_from_search = false;
    }

    pub fn open_in_browser(&self) {
        if let Some(detail) = &self.detail {
            if let Some(html) = &detail.html_body {
                if let Err(e) = open_in_browser(html) {
                    eprintln!("Failed to open browser: {}", e);
                }
            }
        }
    }

    pub fn scroll_detail_up(&mut self) {
        self.detail_scroll = self.detail_scroll.saturating_sub(3);
    }

    pub fn scroll_detail_down(&mut self) {
        self.detail_scroll = self.detail_scroll.saturating_add(3);
    }

    pub fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();

        // Clear background
        let bg_block = Block::default().style(Style::default().bg(theme::BG));
        frame.render_widget(bg_block, area);

        // Main layout: content + status bar
        let [content_area, status_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);

        match self.view {
            ViewMode::List => self.render_list(frame, content_area),
            ViewMode::Detail => self.render_detail_with_peek(frame, content_area),
            ViewMode::Search => self.render_search(frame, content_area),
        }

        self.render_status_bar(frame, status_area);
    }

    fn render_list(&mut self, frame: &mut Frame, area: Rect) {
        let visible_height = area.height as usize;

        // Adjust scroll so selected is visible
        if self.selected < self.list_scroll_offset {
            self.list_scroll_offset = self.selected;
        } else if self.selected >= self.list_scroll_offset + visible_height {
            self.list_scroll_offset = self.selected - visible_height + 1;
        }

        let visible_emails = self
            .emails
            .iter()
            .enumerate()
            .skip(self.list_scroll_offset)
            .take(visible_height);

        for (i, email) in visible_emails {
            let y = area.y + (i - self.list_scroll_offset) as u16;
            if y >= area.y + area.height {
                break;
            }

            let row_area = Rect::new(area.x, y, area.width, 1);
            let is_selected = i == self.selected;

            self.render_email_row(frame, row_area, email, is_selected);
        }

        // Scrollbar
        if self.emails.len() > visible_height {
            let mut scrollbar_state = ScrollbarState::new(self.emails.len())
                .position(self.selected);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .style(Style::default().fg(theme::FG_DIM)),
                area,
                &mut scrollbar_state,
            );
        }
    }

    fn render_email_row(
        &self,
        frame: &mut Frame,
        area: Rect,
        email: &Email,
        is_selected: bool,
    ) {
        let bg = if is_selected {
            theme::BG_SELECTED
        } else {
            theme::BG
        };

        // Clear row with background
        frame.render_widget(Clear, area);
        let bg_block = Block::default().style(Style::default().bg(bg));
        frame.render_widget(bg_block, area);

        let width = area.width as usize;
        if width < 20 {
            return;
        }

        // Layout: [marker 2] [sender 20] [subject flex] [relative_time 14] [exact_time 18]
        let marker_w = 2usize;
        let sender_w = 20usize.min(width / 4);
        let rel_time_w = 14usize;
        let exact_time_w = 18usize;
        let fixed_w = marker_w + sender_w + rel_time_w + exact_time_w + 3; // 3 spaces between
        let subject_w = if width > fixed_w { width - fixed_w } else { 10 };

        let marker = if email.is_unread { "● " } else { "  " };
        let sender = truncate_str(&email.from, sender_w);
        let subject = truncate_str(&email.subject, subject_w);
        let rel_time = relative_time(&email.date);
        let exact_time = email.date.format("%Y-%m-%d %H:%M").to_string();

        let rel_time = format!("{:>width$}", rel_time, width = rel_time_w);
        let exact_time = format!("{:>width$}", exact_time, width = exact_time_w);

        let spans = vec![
            Span::styled(
                marker,
                if email.is_unread {
                    theme::style_unread_marker().bg(bg)
                } else {
                    Style::default().fg(theme::FG_DIM).bg(bg)
                },
            ),
            Span::styled(
                format!("{:<width$}", sender, width = sender_w),
                if is_selected {
                    theme::style_sender_selected()
                } else {
                    theme::style_sender()
                }
                .bg(bg),
            ),
            Span::styled(" ", Style::default().bg(bg)),
            Span::styled(
                format!("{:<width$}", subject, width = subject_w),
                if email.is_unread {
                    theme::style_subject().bg(bg).add_modifier(Modifier::BOLD)
                } else {
                    theme::style_subject().bg(bg)
                },
            ),
            Span::styled(" ", Style::default().bg(bg)),
            Span::styled(rel_time, theme::style_time_relative().bg(bg)),
            Span::styled(" ", Style::default().bg(bg)),
            Span::styled(exact_time, theme::style_time_exact().bg(bg)),
        ];

        let line = Line::from(spans);
        frame.render_widget(Paragraph::new(line), area);
    }

    fn render_detail_with_peek(&mut self, frame: &mut Frame, area: Rect) {
        // Split: narrow left peek of list + wide right detail
        let [peek_area, detail_area] = Layout::horizontal([
            Constraint::Length(LIST_PEEK_WIDTH),
            Constraint::Min(1),
        ])
        .areas(area);

        // Render peek of list (dimmed/truncated)
        self.render_list_peek(frame, peek_area);

        // Render detail panel
        self.render_detail(frame, detail_area);
    }

    fn render_list_peek(&self, frame: &mut Frame, area: Rect) {
        let bg_block = Block::default().style(Style::default().bg(theme::BG));
        frame.render_widget(bg_block, area);

        let visible_height = area.height as usize;
        let visible_emails = self
            .emails
            .iter()
            .enumerate()
            .skip(self.list_scroll_offset)
            .take(visible_height);

        for (i, email) in visible_emails {
            let y = area.y + (i - self.list_scroll_offset) as u16;
            if y >= area.y + area.height {
                break;
            }

            let row_area = Rect::new(area.x, y, area.width, 1);
            let is_selected = i == self.selected;
            let bg = if is_selected {
                theme::BG_SELECTED
            } else {
                theme::BG
            };

            let marker = if email.is_unread { "●" } else { " " };
            let peek_text = truncate_str(&email.from, (area.width as usize).saturating_sub(2));

            let spans = vec![
                Span::styled(
                    marker,
                    if email.is_unread {
                        theme::style_unread_marker().bg(bg)
                    } else {
                        Style::default().fg(theme::FG_DIM).bg(bg)
                    },
                ),
                Span::styled(
                    format!(" {}", peek_text),
                    Style::default().fg(theme::FG_DIM).bg(bg),
                ),
            ];

            frame.render_widget(Paragraph::new(Line::from(spans)), row_area);
        }
    }

    fn render_detail(&self, frame: &mut Frame, area: Rect) {
        let detail_block = Block::default()
            .borders(Borders::LEFT)
            .border_style(Style::default().fg(theme::FG_DIM))
            .style(Style::default().bg(theme::BG_HEADER));
        let inner = detail_block.inner(area);
        frame.render_widget(detail_block, area);

        if let Some(content) = &self.detail {
            let mut lines: Vec<Line> = Vec::new();

            // Headers
            lines.push(Line::from(vec![
                Span::styled("From:    ", theme::style_detail_header_label()),
                Span::styled(&content.from, theme::style_detail_from()),
            ]));
            lines.push(Line::from(vec![
                Span::styled("To:      ", theme::style_detail_header_label()),
                Span::styled(&content.to, theme::style_detail_header_value()),
            ]));
            lines.push(Line::from(vec![
                Span::styled("Subject: ", theme::style_detail_header_label()),
                Span::styled(&content.subject, theme::style_detail_subject()),
            ]));
            lines.push(Line::from(vec![
                Span::styled("Date:    ", theme::style_detail_header_label()),
                Span::styled(
                    format!(
                        "{}  ({})",
                        content.date.format("%a, %d %b %Y %H:%M:%S"),
                        relative_time(&content.date)
                    ),
                    theme::style_detail_header_value(),
                ),
            ]));

            // Separator
            let sep = "─".repeat(inner.width as usize);
            lines.push(Line::from(Span::styled(
                sep,
                Style::default().fg(theme::FG_DIM),
            )));
            lines.push(Line::from(""));

            // Body text
            for line_text in content.text_body.lines() {
                lines.push(Line::from(Span::styled(
                    line_text.to_string(),
                    theme::style_detail_body(),
                )));
            }

            let paragraph = Paragraph::new(lines)
                .scroll((self.detail_scroll, 0))
                .wrap(Wrap { trim: false })
                .style(Style::default().bg(theme::BG_HEADER));

            frame.render_widget(paragraph, inner);

            // Scrollbar for detail
            let total_lines = content.text_body.lines().count() + 6;
            if total_lines > inner.height as usize {
                let mut scrollbar_state = ScrollbarState::new(total_lines)
                    .position(self.detail_scroll as usize);
                frame.render_stateful_widget(
                    Scrollbar::new(ScrollbarOrientation::VerticalRight)
                        .style(Style::default().fg(theme::FG_DIM)),
                    inner,
                    &mut scrollbar_state,
                );
            }
        }
    }

    fn render_search(&mut self, frame: &mut Frame, area: Rect) {
        let [search_area, results_area] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).areas(area);

        // Search input box
        let search_text = format!("/ {}", self.search_query);
        let search_input = Paragraph::new(search_text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme::SENDER_COLOR))
                    .title(" Search ")
                    .title_style(Style::default().fg(theme::SENDER_COLOR))
                    .style(Style::default().bg(theme::BG_HEADER)),
            )
            .style(Style::default().fg(theme::FG_TEXT).bg(theme::BG_HEADER));
        frame.render_widget(search_input, search_area);

        // Results list
        let visible_height = results_area.height as usize;

        if self.search_selected < self.search_scroll_offset {
            self.search_scroll_offset = self.search_selected;
        } else if self.search_selected >= self.search_scroll_offset + visible_height {
            self.search_scroll_offset = self.search_selected - visible_height + 1;
        }

        if self.search_results.is_empty() {
            let msg = if self.search_query.is_empty() {
                "Type a search query and press Enter"
            } else {
                "No results"
            };
            let p = Paragraph::new(msg)
                .style(Style::default().fg(theme::FG_DIM).bg(theme::BG));
            frame.render_widget(p, results_area);
        } else {
            let visible_emails = self
                .search_results
                .iter()
                .enumerate()
                .skip(self.search_scroll_offset)
                .take(visible_height);

            for (i, email) in visible_emails {
                let y = results_area.y + (i - self.search_scroll_offset) as u16;
                if y >= results_area.y + results_area.height {
                    break;
                }
                let row_area = Rect::new(results_area.x, y, results_area.width, 1);
                let is_selected = i == self.search_selected;
                self.render_email_row(frame, row_area, email, is_selected);
            }

            if self.search_results.len() > visible_height {
                let mut scrollbar_state = ScrollbarState::new(self.search_results.len())
                    .position(self.search_selected);
                frame.render_stateful_widget(
                    Scrollbar::new(ScrollbarOrientation::VerticalRight)
                        .style(Style::default().fg(theme::FG_DIM)),
                    results_area,
                    &mut scrollbar_state,
                );
            }
        }
    }

    fn render_status_bar(&self, frame: &mut Frame, area: Rect) {
        let (keys, info) = match self.view {
            ViewMode::List => (
                vec![
                    ("j/k", "navigate"),
                    ("Enter", "open"),
                    ("/", "search"),
                    ("q", "quit"),
                ],
                format!(
                    " {} emails | {}",
                    self.emails.len(),
                    if self.status_msg.is_empty() {
                        "jamail"
                    } else {
                        &self.status_msg
                    }
                ),
            ),
            ViewMode::Detail => (
                vec![
                    ("Esc", "back"),
                    ("j/k", "scroll"),
                    ("v", "browser"),
                    ("q", "quit"),
                ],
                if self.detail.as_ref().and_then(|d| d.html_body.as_ref()).is_some() {
                    " HTML available".to_string()
                } else {
                    " Plain text".to_string()
                },
            ),
            ViewMode::Search => (
                vec![
                    ("Esc", "back"),
                    ("Enter", "search/open"),
                    ("j/k", "navigate"),
                ],
                format!(" {} results", self.search_results.len()),
            ),
        };

        let mut spans = Vec::new();
        for (i, (key, desc)) in keys.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled("  ", theme::style_status_bar()));
            }
            spans.push(Span::styled(
                format!(" {} ", key),
                theme::style_status_key(),
            ));
            spans.push(Span::styled(
                format!(" {}", desc),
                theme::style_status_bar(),
            ));
        }

        // Right-align info
        let keys_width: usize = spans.iter().map(|s| s.content.len()).sum();
        let padding = (area.width as usize).saturating_sub(keys_width + info.len());
        spans.push(Span::styled(
            " ".repeat(padding),
            theme::style_status_bar(),
        ));
        spans.push(Span::styled(info, theme::style_status_bar()));

        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

fn truncate_str(s: &str, max_width: usize) -> String {
    use unicode_width::UnicodeWidthChar;

    if max_width == 0 {
        return String::new();
    }

    let mut width = 0;
    let mut result = String::new();
    let mut needs_ellipsis = false;

    for ch in s.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if width + ch_width > max_width {
            needs_ellipsis = true;
            break;
        }
        width += ch_width;
        result.push(ch);
    }

    if needs_ellipsis && max_width > 1 {
        // Remove chars from the end until we have room for '…' (width 1)
        while width >= max_width {
            if let Some(ch) = result.pop() {
                width -= ch.width().unwrap_or(0);
            } else {
                break;
            }
        }
        result.push('…');
    }

    result
}
