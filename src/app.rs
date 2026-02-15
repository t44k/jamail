use crate::db::{AttachmentMeta, MailDb};
use crate::mail::{Email, EmailContent, FolderInfo, open_in_browser, relative_time};
use crate::theme;
use crate::thread::{DisplayRow, ThreadedView, build_threads, rebuild_rows};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
    },
};
use ratatui_image::{StatefulImage, picker::Picker, protocol::StatefulProtocol};
use regex::Regex;
use std::collections::{HashMap, HashSet};

const LIST_PEEK_WIDTH: u16 = 12;

#[derive(Clone, PartialEq, Eq)]
pub enum ViewMode {
    FolderSelect,
    List,
    Detail,
    Search,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DetailMode {
    Text,
    Attachments,
    Links,
}

pub struct LinkInfo {
    pub url: String,
    /// For <a href> tags: display as "LinkText [url]" instead of raw HTML
    pub display_text: Option<String>,
    pub line: usize,
    pub col_start: usize,
    pub col_end: usize,
}

pub struct Selection {
    pub start_col: u16,
    pub start_row: u16,
    pub end_col: u16,
    pub end_row: u16,
}

impl Selection {
    pub fn normalized(&self) -> (u16, u16, u16, u16) {
        if self.start_row < self.end_row
            || (self.start_row == self.end_row && self.start_col <= self.end_col)
        {
            (self.start_row, self.start_col, self.end_row, self.end_col)
        } else {
            (self.end_row, self.end_col, self.start_row, self.start_col)
        }
    }
}

#[derive(Clone)]
pub enum FolderTreeRow {
    Account {
        name: String,
    },
    Folder {
        account_name: String,
        folder: FolderInfo,
    },
    Loading,
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
    pub show_raw_headers: bool,
    pub selection: Option<Selection>,
    pub selectable_area: Rect,
    pub threaded_view: ThreadedView,
    pub thread_mode: bool,
    /// ID of the email currently open in detail view (for thread navigation)
    pub detail_id: Option<i64>,
    pub detail_attachments: Vec<AttachmentMeta>,
    pub selected_attachment: usize,
    pub detail_mode: DetailMode,
    pub detail_links: Vec<LinkInfo>,
    pub selected_link: usize,
    pub detail_inner_height: u16,
    pub image_picker: Option<Picker>,
    pub image_preview: Option<StatefulProtocol>,
    pub image_preview_id: Option<i64>,

    // Account/folder context
    pub accounts: Vec<(String, crate::config::JamailAccount)>,
    pub current_account: String,
    pub current_folder: String,

    // Folder selection screen
    pub folder_tree: Vec<FolderTreeRow>,
    pub folder_tree_selected: usize,
    pub folder_tree_expanded: HashSet<String>,
    pub folder_tree_scroll: usize,
    pub account_folders: HashMap<String, Vec<FolderInfo>>,
}

impl App {
    pub fn new(
        emails: Vec<Email>,
        picker: Option<Picker>,
        accounts: Vec<(String, crate::config::JamailAccount)>,
        current_account: String,
        current_folder: String,
    ) -> Self {
        let mut threaded_view = ThreadedView::new();
        threaded_view.threads = build_threads(&emails);
        rebuild_rows(&mut threaded_view);

        // Start with default account expanded
        let mut folder_tree_expanded = HashSet::new();
        folder_tree_expanded.insert(current_account.clone());

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
            show_raw_headers: false,
            selection: None,
            selectable_area: Rect::default(),
            threaded_view,
            thread_mode: true,
            detail_id: None,
            detail_attachments: Vec::new(),
            selected_attachment: 0,
            detail_mode: DetailMode::Text,
            detail_links: Vec::new(),
            selected_link: 0,
            detail_inner_height: 0,
            image_picker: picker,
            image_preview: None,
            image_preview_id: None,
            accounts,
            current_account,
            current_folder,
            folder_tree: Vec::new(),
            folder_tree_selected: 0,
            folder_tree_expanded,
            folder_tree_scroll: 0,
            account_folders: HashMap::new(),
        }
    }

    pub fn list_len(&self) -> usize {
        if self.thread_mode {
            self.threaded_view.rows.len()
        } else {
            self.emails.len()
        }
    }

    /// Resolve the current selection to an Email reference.
    pub fn selected_email(&self) -> Option<&Email> {
        if self.thread_mode {
            let row = self.threaded_view.rows.get(self.selected)?;
            match row {
                DisplayRow::ThreadSummary { thread_idx } => {
                    let thread = &self.threaded_view.threads[*thread_idx];
                    thread.email_indices.first().map(|&idx| &self.emails[idx])
                }
                DisplayRow::ThreadEmail {
                    thread_idx,
                    email_idx,
                } => {
                    let idx = self.threaded_view.threads[*thread_idx].email_indices[*email_idx];
                    Some(&self.emails[idx])
                }
            }
        } else {
            self.emails.get(self.selected)
        }
    }

    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.list_len() {
            self.selected += 1;
        }
    }

    pub fn open_detail(&mut self, db: &MailDb) {
        self.detail_from_search = matches!(self.view, ViewMode::Search);

        let id = match self.view {
            ViewMode::Search => {
                if let Some(email) = self.search_results.get(self.search_selected) {
                    email.id
                } else {
                    return;
                }
            }
            _ => {
                if self.thread_mode {
                    let row = match self.threaded_view.rows.get(self.selected) {
                        Some(r) => r.clone(),
                        None => return,
                    };
                    match row {
                        DisplayRow::ThreadSummary { thread_idx } => {
                            let thread = &self.threaded_view.threads[thread_idx];
                            self.emails[thread.email_indices[0]].id
                        }
                        DisplayRow::ThreadEmail {
                            thread_idx,
                            email_idx,
                        } => {
                            let idx = self.threaded_view.threads[thread_idx].email_indices[email_idx];
                            self.emails[idx].id
                        }
                    }
                } else if let Some(email) = self.emails.get(self.selected) {
                    email.id
                } else {
                    return;
                }
            }
        };

        match db.get_email_content(id) {
            Ok(Some(content)) => {
                self.detail_links = extract_links(&content.text_body);
                self.selected_link = 0;
                self.detail_mode = DetailMode::Text;
                self.detail = Some(content);
                self.detail_scroll = 0;
                self.detail_id = Some(id);
                self.detail_attachments = db.get_attachments_meta(id).unwrap_or_default();
                self.selected_attachment = 0;
                self.view = ViewMode::Detail;
                // Mark as read in DB and local lists
                let _ = db.mark_read(id);
                // Update in flat email list
                if let Some(e) = self.emails.iter_mut().find(|e| e.id == id) {
                    e.is_unread = false;
                }
                // Update thread unread counts
                for thread in &mut self.threaded_view.threads {
                    if thread.email_indices.iter().any(|&idx| self.emails[idx].id == id) {
                        thread.unread_count = thread
                            .email_indices
                            .iter()
                            .filter(|&&idx| self.emails[idx].is_unread)
                            .count();
                        break;
                    }
                }
                // Update list selection so the peek view highlights the right row
                self.update_selected_for_id(id);
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
        match db.search_emails(
            &self.current_account,
            &self.current_folder,
            &self.search_query,
        ) {
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
        // Remember current selection by ID
        let selected_id = self.selected_email().map(|e| e.id);

        if let Ok(emails) = db.get_email_list(&self.current_account, &self.current_folder) {
            self.emails = emails;

            // Rebuild threads, preserving expansion state
            let expanded = std::mem::take(&mut self.threaded_view.expanded);
            self.threaded_view.threads = build_threads(&self.emails);
            self.threaded_view.expanded = expanded;
            rebuild_rows(&mut self.threaded_view);

            // Restore selection by ID
            if let Some(id) = selected_id {
                if self.thread_mode {
                    let pos = self.threaded_view.rows.iter().position(|row| match row {
                        DisplayRow::ThreadSummary { thread_idx } => self.threaded_view.threads
                            [*thread_idx]
                            .email_indices
                            .iter()
                            .any(|&idx| self.emails[idx].id == id),
                        DisplayRow::ThreadEmail {
                            thread_idx,
                            email_idx,
                        } => {
                            let idx = self.threaded_view.threads[*thread_idx].email_indices
                                [*email_idx];
                            self.emails[idx].id == id
                        }
                    });
                    if let Some(idx) = pos {
                        self.selected = idx;
                    }
                } else if let Some(idx) = self.emails.iter().position(|e| e.id == id) {
                    self.selected = idx;
                }
            }

            let len = self.list_len();
            if self.selected >= len && len > 0 {
                self.selected = len - 1;
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
        self.show_raw_headers = false;
        self.detail_attachments.clear();
        self.selected_attachment = 0;
        self.detail_mode = DetailMode::Text;
        self.detail_links.clear();
        self.selected_link = 0;
        self.image_preview = None;
        self.image_preview_id = None;
    }

    pub fn toggle_thread_expand(&mut self) {
        if !self.thread_mode {
            return;
        }
        let row = match self.threaded_view.rows.get(self.selected) {
            Some(r) => r.clone(),
            None => return,
        };
        let thread_idx = match row {
            DisplayRow::ThreadSummary { thread_idx } => thread_idx,
            DisplayRow::ThreadEmail { thread_idx, .. } => thread_idx,
        };
        let thread = &self.threaded_view.threads[thread_idx];
        if thread.message_count <= 1 {
            return;
        }
        let id = thread.id.clone();
        if self.threaded_view.expanded.contains(&id) {
            self.threaded_view.expanded.remove(&id);
        } else {
            self.threaded_view.expanded.insert(id);
        }
        let was_summary = matches!(row, DisplayRow::ThreadSummary { .. });
        rebuild_rows(&mut self.threaded_view);
        if was_summary
            && let Some(pos) = self.threaded_view.rows.iter().position(
                |r| matches!(r, DisplayRow::ThreadSummary { thread_idx: ti } if *ti == thread_idx),
            )
        {
            self.selected = pos;
        }
    }

    pub fn next_thread(&mut self) {
        if !self.thread_mode {
            return;
        }
        for i in (self.selected + 1)..self.threaded_view.rows.len() {
            if matches!(self.threaded_view.rows[i], DisplayRow::ThreadSummary { .. }) {
                self.selected = i;
                return;
            }
        }
    }

    pub fn prev_thread(&mut self) {
        if !self.thread_mode || self.selected == 0 {
            return;
        }
        if let Some(DisplayRow::ThreadEmail { thread_idx, .. }) =
            self.threaded_view.rows.get(self.selected)
        {
            let ti = *thread_idx;
            for i in (0..self.selected).rev() {
                if matches!(self.threaded_view.rows[i], DisplayRow::ThreadSummary { thread_idx } if thread_idx == ti)
                {
                    self.selected = i;
                    return;
                }
            }
        }
        for i in (0..self.selected).rev() {
            if matches!(self.threaded_view.rows[i], DisplayRow::ThreadSummary { .. }) {
                self.selected = i;
                return;
            }
        }
    }

    pub fn toggle_thread_mode(&mut self) {
        let current_id = self.selected_email().map(|e| e.id);
        self.thread_mode = !self.thread_mode;

        if let Some(id) = current_id {
            if self.thread_mode {
                let pos = self.threaded_view.rows.iter().position(|row| match row {
                    DisplayRow::ThreadSummary { thread_idx } => self.threaded_view.threads
                        [*thread_idx]
                        .email_indices
                        .iter()
                        .any(|&idx| self.emails[idx].id == id),
                    DisplayRow::ThreadEmail {
                        thread_idx,
                        email_idx,
                    } => {
                        let idx =
                            self.threaded_view.threads[*thread_idx].email_indices[*email_idx];
                        self.emails[idx].id == id
                    }
                });
                if let Some(idx) = pos {
                    self.selected = idx;
                }
            } else if let Some(idx) = self.emails.iter().position(|e| e.id == id) {
                self.selected = idx;
            }
        }

        let len = self.list_len();
        if self.selected >= len && len > 0 {
            self.selected = len - 1;
        }
        self.list_scroll_offset = 0;
    }

    /// Navigate to the next message in the same thread (from detail view).
    pub fn next_in_thread(&mut self, db: &MailDb) {
        let id = match self.detail_id {
            Some(u) => u,
            None => return,
        };
        let next_id = self.threaded_view.threads.iter().find_map(|thread| {
            let pos = thread
                .email_indices
                .iter()
                .position(|&idx| self.emails[idx].id == id)?;
            if pos + 1 < thread.email_indices.len() {
                Some(self.emails[thread.email_indices[pos + 1]].id)
            } else {
                None
            }
        });
        if let Some(nid) = next_id {
            self.open_detail_by_id(db, nid);
        }
    }

    /// Navigate to the previous message in the same thread (from detail view).
    pub fn prev_in_thread(&mut self, db: &MailDb) {
        let id = match self.detail_id {
            Some(u) => u,
            None => return,
        };
        let prev_id = self.threaded_view.threads.iter().find_map(|thread| {
            let pos = thread
                .email_indices
                .iter()
                .position(|&idx| self.emails[idx].id == id)?;
            if pos > 0 {
                Some(self.emails[thread.email_indices[pos - 1]].id)
            } else {
                None
            }
        });
        if let Some(pid) = prev_id {
            self.open_detail_by_id(db, pid);
        }
    }

    /// Open a specific email by ID in detail view.
    fn open_detail_by_id(&mut self, db: &MailDb, id: i64) {
        match db.get_email_content(id) {
            Ok(Some(content)) => {
                self.detail_links = extract_links(&content.text_body);
                self.selected_link = 0;
                self.detail_mode = DetailMode::Text;
                self.detail = Some(content);
                self.detail_scroll = 0;
                self.detail_id = Some(id);
                self.detail_attachments = db.get_attachments_meta(id).unwrap_or_default();
                self.selected_attachment = 0;
                self.show_raw_headers = false;
                let _ = db.mark_read(id);
                if let Some(e) = self.emails.iter_mut().find(|e| e.id == id) {
                    e.is_unread = false;
                }
                for thread in &mut self.threaded_view.threads {
                    if thread.email_indices.iter().any(|&idx| self.emails[idx].id == id) {
                        thread.unread_count = thread
                            .email_indices
                            .iter()
                            .filter(|&&idx| self.emails[idx].is_unread)
                            .count();
                        break;
                    }
                }
                self.update_selected_for_id(id);
            }
            Ok(None) => {
                self.status_msg = "Email not found in cache".to_string();
            }
            Err(e) => {
                self.status_msg = format!("Error: {}", e);
            }
        }
    }

    /// Update `self.selected` to point at the row for the given ID.
    fn update_selected_for_id(&mut self, id: i64) {
        if self.thread_mode {
            for thread in &self.threaded_view.threads {
                if thread
                    .email_indices
                    .iter()
                    .any(|&idx| self.emails[idx].id == id)
                {
                    if thread.message_count > 1 && !self.threaded_view.expanded.contains(&thread.id)
                    {
                        self.threaded_view.expanded.insert(thread.id.clone());
                        rebuild_rows(&mut self.threaded_view);
                    }
                    break;
                }
            }
            if let Some(pos) = self.threaded_view.rows.iter().position(|row| match row {
                DisplayRow::ThreadEmail {
                    thread_idx,
                    email_idx,
                } => {
                    let idx =
                        self.threaded_view.threads[*thread_idx].email_indices[*email_idx];
                    self.emails[idx].id == id
                }
                DisplayRow::ThreadSummary { thread_idx } => {
                    let thread = &self.threaded_view.threads[*thread_idx];
                    thread.message_count == 1
                        && self.emails[thread.email_indices[0]].id == id
                }
            }) {
                self.selected = pos;
            }
        } else if let Some(pos) = self.emails.iter().position(|e| e.id == id) {
            self.selected = pos;
        }
    }

    fn detail_thread_info(&self) -> Option<(usize, usize)> {
        let id = self.detail_id?;
        for thread in &self.threaded_view.threads {
            if thread.message_count <= 1 {
                continue;
            }
            if let Some(pos) = thread
                .email_indices
                .iter()
                .position(|&idx| self.emails[idx].id == id)
            {
                return Some((pos + 1, thread.message_count));
            }
        }
        None
    }

    pub fn open_in_browser(&self) {
        if let Some(detail) = &self.detail
            && let Some(html) = &detail.html_body
            && let Err(e) = open_in_browser(html)
        {
            eprintln!("Failed to open browser: {}", e);
        }
    }

    pub fn toggle_raw_headers(&mut self) {
        self.show_raw_headers = !self.show_raw_headers;
        self.detail_scroll = 0;
        if self.show_raw_headers {
            self.detail_mode = DetailMode::Text;
        }
    }

    pub fn next_attachment(&mut self) {
        if !self.detail_attachments.is_empty() {
            self.selected_attachment =
                (self.selected_attachment + 1) % self.detail_attachments.len();
        }
    }

    pub fn prev_attachment(&mut self) {
        if !self.detail_attachments.is_empty() {
            if self.selected_attachment == 0 {
                self.selected_attachment = self.detail_attachments.len() - 1;
            } else {
                self.selected_attachment -= 1;
            }
        }
    }

    pub fn save_attachment(&mut self, db: &MailDb) {
        if self.detail_attachments.is_empty() {
            return;
        }
        let att = &self.detail_attachments[self.selected_attachment];
        let data = match db.get_attachment_data(att.id) {
            Ok(Some(d)) => d,
            Ok(None) => {
                self.status_msg = "Attachment data not found".to_string();
                return;
            }
            Err(e) => {
                self.status_msg = format!("Error: {}", e);
                return;
            }
        };

        let downloads = dirs::home_dir()
            .map(|h| h.join("Downloads"))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let _ = std::fs::create_dir_all(&downloads);

        let path = dedup_filename(&downloads, &att.filename);
        match std::fs::write(&path, &data) {
            Ok(_) => {
                self.status_msg = format!(
                    "Saved {} ({})",
                    path.file_name().unwrap_or_default().to_string_lossy(),
                    format_size(data.len())
                );
            }
            Err(e) => {
                self.status_msg = format!("Save failed: {}", e);
            }
        }
    }

    pub fn update_image_preview(&mut self, db: &MailDb) {
        if self.detail_mode != DetailMode::Attachments || self.detail_attachments.is_empty() {
            self.image_preview = None;
            self.image_preview_id = None;
            return;
        }

        let att = &self.detail_attachments[self.selected_attachment];

        if self.image_preview_id == Some(att.id) {
            return;
        }

        if !att.mime_type.starts_with("image/") {
            self.image_preview = None;
            self.image_preview_id = None;
            return;
        }

        let att_id = att.id;

        let picker = match &self.image_picker {
            Some(p) => p,
            None => {
                self.image_preview = None;
                self.image_preview_id = None;
                return;
            }
        };

        let data = match db.get_attachment_data(att_id) {
            Ok(Some(d)) => d,
            _ => {
                self.image_preview = None;
                self.image_preview_id = None;
                return;
            }
        };

        match image::load_from_memory(&data) {
            Ok(img) => {
                self.image_preview = Some(picker.new_resize_protocol(img));
                self.image_preview_id = Some(att_id);
            }
            Err(_) => {
                self.image_preview = None;
                self.image_preview_id = None;
                self.status_msg = "Failed to decode image".to_string();
            }
        }
    }

    pub fn cycle_detail_mode(&mut self) {
        if self.show_raw_headers {
            return;
        }
        let has_attachments = !self.detail_attachments.is_empty();
        let has_links = !self.detail_links.is_empty();
        self.detail_mode = match self.detail_mode {
            DetailMode::Text => {
                if has_attachments {
                    DetailMode::Attachments
                } else if has_links {
                    DetailMode::Links
                } else {
                    DetailMode::Text
                }
            }
            DetailMode::Attachments => {
                if has_links {
                    DetailMode::Links
                } else {
                    DetailMode::Text
                }
            }
            DetailMode::Links => DetailMode::Text,
        };
    }

    pub fn next_link(&mut self) {
        if !self.detail_links.is_empty() {
            self.selected_link = (self.selected_link + 1) % self.detail_links.len();
            self.scroll_to_link();
        }
    }

    pub fn prev_link(&mut self) {
        if !self.detail_links.is_empty() {
            if self.selected_link == 0 {
                self.selected_link = self.detail_links.len() - 1;
            } else {
                self.selected_link -= 1;
            }
            self.scroll_to_link();
        }
    }

    fn scroll_to_link(&mut self) {
        if let Some(link) = self.detail_links.get(self.selected_link) {
            let header_lines = if self.detail_attachments.is_empty() {
                6
            } else {
                7
            } + if self.detail_thread_info().is_some() {
                1
            } else {
                0
            } + 1;
            let target = (header_lines + link.line) as u16;
            let visible_start = self.detail_scroll;
            let visible_end = self.detail_scroll.saturating_add(self.detail_inner_height);
            if target < visible_start || target >= visible_end {
                self.detail_scroll = target.saturating_sub(self.detail_inner_height / 3);
            }
        }
    }

    pub fn open_selected_link(&self) {
        if let Some(link) = self.detail_links.get(self.selected_link) {
            let _ = std::process::Command::new("xdg-open")
                .arg(&link.url)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }

    pub fn prev_in_list(&mut self, db: &MailDb) {
        if self.selected > 0 {
            self.selected -= 1;
            let email = self.selected_email().cloned();
            if let Some(email) = email {
                self.open_detail_by_id(db, email.id);
            }
        }
    }

    pub fn next_in_list(&mut self, db: &MailDb) {
        if self.selected + 1 < self.list_len() {
            self.selected += 1;
            let email = self.selected_email().cloned();
            if let Some(email) = email {
                self.open_detail_by_id(db, email.id);
            }
        }
    }

    pub fn start_selection(&mut self, col: u16, row: u16) {
        let a = self.selectable_area;
        if a.width == 0 || a.height == 0 {
            return;
        }
        if col < a.x || col >= a.x + a.width || row < a.y || row >= a.y + a.height {
            return;
        }
        self.selection = Some(Selection {
            start_col: col,
            start_row: row,
            end_col: col,
            end_row: row,
        });
    }

    pub fn update_selection(&mut self, col: u16, row: u16) {
        if let Some(sel) = &mut self.selection {
            let a = self.selectable_area;
            sel.end_col = col.clamp(a.x, a.x + a.width.saturating_sub(1));
            sel.end_row = row.clamp(a.y, a.y + a.height.saturating_sub(1));
        }
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    pub fn render_selection(&self, frame: &mut Frame) {
        let Some(sel) = &self.selection else { return };
        let a = self.selectable_area;
        if a.width == 0 || a.height == 0 {
            return;
        }
        let (sr, sc, er, ec) = sel.normalized();
        let buf = frame.buffer_mut();
        for row in sr..=er {
            if row < a.y || row >= a.y + a.height {
                continue;
            }
            let col_start = if row == sr { sc } else { a.x };
            let col_end = if row == er { ec } else { a.x + a.width - 1 };
            for col in col_start..=col_end {
                if col < a.x || col >= a.x + a.width {
                    continue;
                }
                if let Some(cell) = buf.cell_mut(Position::new(col, row)) {
                    cell.set_style(Style::default().bg(theme::SELECTION_BG));
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

    pub fn page_up(&mut self, page_size: usize) {
        self.selected = self.selected.saturating_sub(page_size);
    }

    pub fn page_down(&mut self, page_size: usize) {
        let len = self.list_len();
        if len > 0 {
            self.selected = (self.selected + page_size).min(len - 1);
        }
    }

    pub fn go_home(&mut self) {
        self.selected = 0;
    }

    pub fn go_end(&mut self) {
        let len = self.list_len();
        if len > 0 {
            self.selected = len - 1;
        }
    }

    pub fn scroll_detail_page_up(&mut self, page_size: u16) {
        self.detail_scroll = self.detail_scroll.saturating_sub(page_size);
    }

    pub fn scroll_detail_page_down(&mut self, page_size: u16) {
        self.detail_scroll = self.detail_scroll.saturating_add(page_size);
    }

    pub fn scroll_detail_home(&mut self) {
        self.detail_scroll = 0;
    }

    pub fn scroll_detail_end(&mut self) {
        self.detail_scroll = u16::MAX / 2;
    }

    // ── Folder selection ────────────────────────────────────────────

    pub fn enter_folder_select(&mut self) {
        // Auto-expand the current account so folders are visible immediately
        self.folder_tree_expanded
            .insert(self.current_account.clone());
        self.rebuild_folder_tree();
        self.folder_tree_selected = 0;
        self.folder_tree_scroll = 0;
        self.view = ViewMode::FolderSelect;
    }

    pub fn rebuild_folder_tree(&mut self) {
        self.folder_tree.clear();
        for (name, _account) in &self.accounts {
            self.folder_tree
                .push(FolderTreeRow::Account { name: name.clone() });
            if self.folder_tree_expanded.contains(name) {
                let folders = self.account_folders.get(name).cloned().unwrap_or_default();
                if folders.is_empty() {
                    self.folder_tree.push(FolderTreeRow::Loading);
                } else {
                    for folder in folders {
                        self.folder_tree.push(FolderTreeRow::Folder {
                            account_name: name.clone(),
                            folder,
                        });
                    }
                }
            }
        }
    }

    pub fn folder_tree_toggle_expand(&mut self) {
        if let Some(row) = self.folder_tree.get(self.folder_tree_selected) {
            match row {
                FolderTreeRow::Account { name } => {
                    let name = name.clone();
                    if self.folder_tree_expanded.contains(&name) {
                        self.folder_tree_expanded.remove(&name);
                    } else {
                        self.folder_tree_expanded.insert(name);
                    }
                    self.rebuild_folder_tree();
                }
                FolderTreeRow::Folder { .. } | FolderTreeRow::Loading => {}
            }
        }
    }

    pub fn folder_tree_select(&mut self) -> Option<(String, String)> {
        if let Some(row) = self.folder_tree.get(self.folder_tree_selected) {
            match row {
                FolderTreeRow::Account { .. } => {
                    self.folder_tree_toggle_expand();
                    None
                }
                FolderTreeRow::Folder {
                    account_name,
                    folder,
                } => Some((account_name.clone(), folder.name.clone())),
                FolderTreeRow::Loading => None,
            }
        } else {
            None
        }
    }

    pub fn folder_tree_up(&mut self) {
        if self.folder_tree_selected > 0 {
            self.folder_tree_selected -= 1;
        }
    }

    pub fn folder_tree_down(&mut self) {
        if self.folder_tree_selected + 1 < self.folder_tree.len() {
            self.folder_tree_selected += 1;
        }
    }

    /// Get the short display name for the current folder (last path component).
    fn folder_short_name(&self) -> String {
        folder_display_name(&self.current_folder)
    }

    // ── Rendering ───────────────────────────────────────────────────

    pub fn render(&mut self, frame: &mut Frame) {
        self.selectable_area = Rect::default();
        let area = frame.area();

        let bg_block = Block::default().style(Style::default().bg(theme::BG));
        frame.render_widget(bg_block, area);

        let [content_area, status_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);

        match self.view {
            ViewMode::FolderSelect => self.render_folder_select(frame, content_area),
            ViewMode::List => self.render_list(frame, content_area),
            ViewMode::Detail => self.render_detail_with_peek(frame, content_area),
            ViewMode::Search => self.render_search(frame, content_area),
        }

        self.render_status_bar(frame, status_area);
    }

    fn render_list(&mut self, frame: &mut Frame, area: Rect) {
        // Split: vertical folder label | email list
        let folder_label_w = 3u16;
        let [label_area, list_area] =
            Layout::horizontal([Constraint::Length(folder_label_w), Constraint::Min(1)])
                .areas(area);

        self.render_folder_label(frame, label_area);

        if self.thread_mode {
            self.render_threaded_list(frame, list_area);
        } else {
            self.render_flat_list(frame, list_area);
        }
    }

    fn render_folder_label(&self, frame: &mut Frame, area: Rect) {
        let name = self.folder_short_name();
        let style = Style::default().fg(theme::THREAD_INDICATOR).bg(theme::BG);

        // Render each character vertically
        for (i, ch) in name.chars().enumerate() {
            let y = area.y + i as u16;
            if y >= area.y + area.height {
                break;
            }
            let row_area = Rect::new(area.x, y, area.width, 1);
            // Center the char in the 3-wide column
            let label = format!(" {} ", ch);
            frame.render_widget(Paragraph::new(label).style(style), row_area);
        }

        // Fill remaining rows with background
        let name_len = name.chars().count() as u16;
        for y in (area.y + name_len)..(area.y + area.height) {
            let row_area = Rect::new(area.x, y, area.width, 1);
            frame.render_widget(
                Paragraph::new("   ").style(Style::default().bg(theme::BG)),
                row_area,
            );
        }
    }

    fn render_flat_list(&mut self, frame: &mut Frame, area: Rect) {
        let visible_height = area.height as usize;
        let has_scrollbar = self.emails.len() > visible_height;
        let content_width = if has_scrollbar {
            area.width.saturating_sub(1)
        } else {
            area.width
        };

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

            let row_area = Rect::new(area.x, y, content_width, 1);
            let is_selected = i == self.selected;

            self.render_email_row(frame, row_area, email, is_selected);
        }

        if has_scrollbar {
            let mut scrollbar_state =
                ScrollbarState::new(self.emails.len()).position(self.selected);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .style(Style::default().fg(theme::FG_DIM)),
                area,
                &mut scrollbar_state,
            );
        }
    }

    fn render_threaded_list(&mut self, frame: &mut Frame, area: Rect) {
        let visible_height = area.height as usize;
        let total = self.threaded_view.rows.len();
        let has_scrollbar = total > visible_height;
        let content_width = if has_scrollbar {
            area.width.saturating_sub(1)
        } else {
            area.width
        };

        if self.selected < self.list_scroll_offset {
            self.list_scroll_offset = self.selected;
        } else if self.selected >= self.list_scroll_offset + visible_height {
            self.list_scroll_offset = self.selected - visible_height + 1;
        }

        for (vi, row_idx) in (self.list_scroll_offset..total)
            .enumerate()
            .take(visible_height)
        {
            let y = area.y + vi as u16;
            if y >= area.y + area.height {
                break;
            }
            let row_area = Rect::new(area.x, y, content_width, 1);
            let is_selected = row_idx == self.selected;
            let row = &self.threaded_view.rows[row_idx];

            match row {
                DisplayRow::ThreadSummary { thread_idx } => {
                    let thread = &self.threaded_view.threads[*thread_idx];
                    self.render_thread_summary_row(frame, row_area, thread, is_selected);
                }
                DisplayRow::ThreadEmail {
                    thread_idx,
                    email_idx,
                } => {
                    let thread = &self.threaded_view.threads[*thread_idx];
                    let email = &self.emails[thread.email_indices[*email_idx]];
                    let is_last = *email_idx == thread.email_indices.len() - 1;
                    self.render_thread_email_row(frame, row_area, email, is_selected, is_last);
                }
            }
        }

        if has_scrollbar {
            let mut scrollbar_state = ScrollbarState::new(total).position(self.selected);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .style(Style::default().fg(theme::FG_DIM)),
                area,
                &mut scrollbar_state,
            );
        }
    }

    fn render_thread_summary_row(
        &self,
        frame: &mut Frame,
        area: Rect,
        thread: &crate::thread::Thread,
        is_selected: bool,
    ) {
        let bg = if is_selected {
            theme::BG_SELECTED
        } else {
            theme::BG
        };

        frame.render_widget(Clear, area);
        let bg_block = Block::default().style(Style::default().bg(bg));
        frame.render_widget(bg_block, area);

        let width = area.width as usize;
        if width < 20 {
            return;
        }

        let has_unread = thread.unread_count > 0;
        let has_attachments = thread.has_attachments;
        let is_expanded = self.threaded_view.expanded.contains(&thread.id);

        let marker_w = 2usize;
        let badge_display_w = 6usize;
        let sender_w = 20usize.min(width / 4);
        let rel_time_w = 14usize;
        let exact_time_w = 18usize;

        let count_badge = if thread.message_count > 1 {
            let arrow = if is_expanded { "▼" } else { "▶" };
            pad_to_display_width(
                &format!("{}{}", arrow, thread.message_count),
                badge_display_w,
            )
        } else {
            " ".repeat(badge_display_w)
        };
        let badge_w = badge_display_w;

        let fixed_w = marker_w + badge_w + sender_w + rel_time_w + exact_time_w + 3;
        let subject_w = if width > fixed_w { width - fixed_w } else { 10 };

        let unread_marker = if has_unread { "●" } else { " " };
        let attach_marker = if has_attachments { "@" } else { " " };
        let subject = truncate_str(&thread.subject, subject_w);
        let sender = truncate_str(&thread.newest_from, sender_w);
        let rel_time = relative_time(&thread.newest_date);
        let exact_time = thread.newest_date.format("%Y-%m-%d %H:%M").to_string();
        let rel_time = format!("{:>width$}", rel_time, width = rel_time_w);
        let exact_time = format!("{:>width$}", exact_time, width = exact_time_w);

        let mut spans = vec![
            Span::styled(
                unread_marker,
                if has_unread {
                    theme::style_unread_marker().bg(bg)
                } else {
                    Style::default().fg(theme::FG_DIM).bg(bg)
                },
            ),
            Span::styled(
                attach_marker,
                if has_attachments {
                    Style::default().fg(theme::ATTACHMENT_COLOR).bg(bg)
                } else {
                    Style::default().fg(theme::FG_DIM).bg(bg)
                },
            ),
            Span::styled(
                count_badge,
                if thread.message_count > 1 {
                    Style::default()
                        .fg(theme::THREAD_INDICATOR)
                        .bg(bg)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().bg(bg)
                },
            ),
        ];

        spans.extend([
            Span::styled(
                pad_to_display_width(&subject, subject_w),
                if has_unread {
                    theme::style_subject().bg(bg).add_modifier(Modifier::BOLD)
                } else {
                    theme::style_subject().bg(bg)
                },
            ),
            Span::styled(" ", Style::default().bg(bg)),
            Span::styled(
                pad_to_display_width(&sender, sender_w),
                if is_selected {
                    theme::style_sender_selected()
                } else {
                    theme::style_sender()
                }
                .bg(bg),
            ),
            Span::styled(" ", Style::default().bg(bg)),
            Span::styled(rel_time, theme::style_time_relative().bg(bg)),
            Span::styled(" ", Style::default().bg(bg)),
            Span::styled(exact_time, theme::style_time_exact().bg(bg)),
        ]);

        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn render_thread_email_row(
        &self,
        frame: &mut Frame,
        area: Rect,
        email: &Email,
        is_selected: bool,
        is_last: bool,
    ) {
        let bg = if is_selected {
            theme::BG_SELECTED
        } else {
            theme::BG
        };

        frame.render_widget(Clear, area);
        let bg_block = Block::default().style(Style::default().bg(bg));
        frame.render_widget(bg_block, area);

        let width = area.width as usize;
        if width < 20 {
            return;
        }

        let branch = if is_last { "  └─ " } else { "  ├─ " };
        let branch_w = 5usize;
        let rel_time_w = 14usize;
        let exact_time_w = 18usize;
        let marker_w = 2usize;
        let fixed_w = marker_w + branch_w + rel_time_w + exact_time_w + 2;
        let sender_w = if width > fixed_w {
            (width - fixed_w).min(40)
        } else {
            10
        };

        let unread_marker = if email.is_unread { "●" } else { " " };
        let attach_marker = if email.has_attachments { "@" } else { " " };
        let sender = truncate_str(&email.from, sender_w);
        let rel_time = relative_time(&email.date);
        let exact_time = email.date.format("%Y-%m-%d %H:%M").to_string();
        let rel_time = format!("{:>width$}", rel_time, width = rel_time_w);
        let exact_time = format!("{:>width$}", exact_time, width = exact_time_w);

        let spans = vec![
            Span::styled(
                unread_marker,
                if email.is_unread {
                    theme::style_unread_marker().bg(bg)
                } else {
                    Style::default().fg(theme::FG_DIM).bg(bg)
                },
            ),
            Span::styled(
                attach_marker,
                if email.has_attachments {
                    Style::default().fg(theme::ATTACHMENT_COLOR).bg(bg)
                } else {
                    Style::default().fg(theme::FG_DIM).bg(bg)
                },
            ),
            Span::styled(branch, Style::default().fg(theme::THREAD_BRANCH).bg(bg)),
            Span::styled(
                pad_to_display_width(&sender, sender_w),
                if is_selected {
                    theme::style_sender_selected()
                } else {
                    theme::style_sender()
                }
                .bg(bg),
            ),
            Span::styled(" ", Style::default().bg(bg)),
            Span::styled(rel_time, theme::style_time_relative().bg(bg)),
            Span::styled(" ", Style::default().bg(bg)),
            Span::styled(exact_time, theme::style_time_exact().bg(bg)),
        ];

        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn render_email_row(&self, frame: &mut Frame, area: Rect, email: &Email, is_selected: bool) {
        let bg = if is_selected {
            theme::BG_SELECTED
        } else {
            theme::BG
        };

        frame.render_widget(Clear, area);
        let bg_block = Block::default().style(Style::default().bg(bg));
        frame.render_widget(bg_block, area);

        let width = area.width as usize;
        if width < 20 {
            return;
        }

        let marker_w = 2usize;
        let sender_w = 20usize.min(width / 4);
        let rel_time_w = 14usize;
        let exact_time_w = 18usize;
        let fixed_w = marker_w + sender_w + rel_time_w + exact_time_w + 3;
        let subject_w = if width > fixed_w { width - fixed_w } else { 10 };

        let unread_marker = if email.is_unread { "●" } else { " " };
        let attach_marker = if email.has_attachments { "@" } else { " " };
        let sender = truncate_str(&email.from, sender_w);
        let subject = truncate_str(&email.subject, subject_w);
        let rel_time = relative_time(&email.date);
        let exact_time = email.date.format("%Y-%m-%d %H:%M").to_string();

        let rel_time = format!("{:>width$}", rel_time, width = rel_time_w);
        let exact_time = format!("{:>width$}", exact_time, width = exact_time_w);

        let spans = vec![
            Span::styled(
                unread_marker,
                if email.is_unread {
                    theme::style_unread_marker().bg(bg)
                } else {
                    Style::default().fg(theme::FG_DIM).bg(bg)
                },
            ),
            Span::styled(
                attach_marker,
                if email.has_attachments {
                    Style::default().fg(theme::ATTACHMENT_COLOR).bg(bg)
                } else {
                    Style::default().fg(theme::FG_DIM).bg(bg)
                },
            ),
            Span::styled(
                pad_to_display_width(&sender, sender_w),
                if is_selected {
                    theme::style_sender_selected()
                } else {
                    theme::style_sender()
                }
                .bg(bg),
            ),
            Span::styled(" ", Style::default().bg(bg)),
            Span::styled(
                pad_to_display_width(&subject, subject_w),
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
        let [peek_area, detail_area] =
            Layout::horizontal([Constraint::Length(LIST_PEEK_WIDTH), Constraint::Min(1)])
                .areas(area);

        self.render_list_peek(frame, peek_area);
        self.render_detail(frame, detail_area);
    }

    fn render_list_peek(&mut self, frame: &mut Frame, area: Rect) {
        let bg_block = Block::default().style(Style::default().bg(theme::BG));
        frame.render_widget(bg_block, area);

        let visible_height = area.height as usize;

        if self.selected < self.list_scroll_offset {
            self.list_scroll_offset = self.selected;
        } else if visible_height > 0 && self.selected >= self.list_scroll_offset + visible_height {
            self.list_scroll_offset = self.selected - visible_height + 1;
        }

        if self.thread_mode {
            let total = self.threaded_view.rows.len();
            for (vi, row_idx) in (self.list_scroll_offset..total)
                .enumerate()
                .take(visible_height)
            {
                let y = area.y + vi as u16;
                if y >= area.y + area.height {
                    break;
                }
                let row_area = Rect::new(area.x, y, area.width, 1);
                let is_selected = row_idx == self.selected;
                let bg = if is_selected {
                    theme::BG_SELECTED
                } else {
                    theme::BG
                };

                let row = &self.threaded_view.rows[row_idx];
                let (marker_str, peek_text) = match row {
                    DisplayRow::ThreadSummary { thread_idx } => {
                        let thread = &self.threaded_view.threads[*thread_idx];
                        let has_unread = thread.unread_count > 0;
                        let marker = if has_unread { "●" } else { " " };
                        let count = if thread.message_count > 1 {
                            format!("[{}]", thread.message_count)
                        } else {
                            String::new()
                        };
                        let text = truncate_str(
                            &thread.subject,
                            (area.width as usize).saturating_sub(2 + count.len()),
                        );
                        ((marker, has_unread), format!("{}{}", count, text))
                    }
                    DisplayRow::ThreadEmail {
                        thread_idx,
                        email_idx,
                    } => {
                        let idx = self.threaded_view.threads[*thread_idx].email_indices[*email_idx];
                        let email = &self.emails[idx];
                        let marker = if email.is_unread { "●" } else { " " };
                        let text =
                            truncate_str(&email.from, (area.width as usize).saturating_sub(3));
                        ((marker, email.is_unread), format!(" {}", text))
                    }
                };

                let spans = vec![
                    Span::styled(
                        marker_str.0,
                        if marker_str.1 {
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
        } else {
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
    }

    fn render_detail(&mut self, frame: &mut Frame, area: Rect) {
        let detail_block = Block::default()
            .borders(Borders::LEFT)
            .border_style(Style::default().fg(theme::FG_DIM))
            .style(Style::default().bg(theme::BG_HEADER));
        let inner = detail_block.inner(area);
        self.selectable_area = inner;
        frame.render_widget(detail_block, area);

        let show_link_preview = self.detail_mode == DetailMode::Links
            && !self.detail_links.is_empty()
            && !self.show_raw_headers;
        let (body_inner, link_preview_area) = if show_link_preview {
            let [body, preview] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).areas(inner);
            (body, Some(preview))
        } else {
            (inner, None)
        };
        self.detail_inner_height = body_inner.height;

        if let Some(content) = &self.detail {
            let mut lines: Vec<Line> = Vec::new();

            if self.show_raw_headers {
                lines.push(Line::from(Span::styled(
                    "── Raw Headers ──",
                    Style::default()
                        .fg(theme::SENDER_COLOR)
                        .add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(""));
                for line_text in content.raw_headers.lines() {
                    lines.push(Line::from(Span::styled(
                        line_text.to_string(),
                        theme::style_detail_body(),
                    )));
                }
            } else {
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

                if let Some((pos, total)) = self.detail_thread_info() {
                    lines.push(Line::from(vec![
                        Span::styled("Thread:  ", theme::style_detail_header_label()),
                        Span::styled(
                            format!("{}/{}", pos, total),
                            Style::default().fg(theme::THREAD_INDICATOR),
                        ),
                        Span::styled("  n/p navigate", Style::default().fg(theme::FG_DIM)),
                    ]));
                }

                if !self.detail_attachments.is_empty() {
                    let mut spans = vec![Span::styled(
                        "Attach:  ",
                        theme::style_detail_header_label(),
                    )];
                    for (i, att) in self.detail_attachments.iter().enumerate() {
                        if i > 0 {
                            spans.push(Span::styled("  ", Style::default()));
                        }
                        let label = format!(
                            "[{}] {} ({})",
                            i + 1,
                            att.filename,
                            format_size(att.size_bytes)
                        );
                        if self.detail_mode == DetailMode::Attachments
                            && i == self.selected_attachment
                        {
                            spans.push(Span::styled(
                                label,
                                Style::default()
                                    .fg(theme::ATTACHMENT_SELECTED)
                                    .bg(theme::ATTACHMENT_SELECTED_BG)
                                    .add_modifier(Modifier::BOLD),
                            ));
                        } else {
                            spans.push(Span::styled(
                                label,
                                Style::default().fg(theme::ATTACHMENT_COLOR),
                            ));
                        }
                    }
                    lines.push(Line::from(spans));
                }

                let sep = "─".repeat(inner.width as usize);
                lines.push(Line::from(Span::styled(
                    sep,
                    Style::default().fg(theme::FG_DIM),
                )));

                match self.detail_mode {
                    DetailMode::Text => {
                        lines.push(Line::from(""));
                    }
                    DetailMode::Attachments => {
                        let indicator = format!(
                            "── ATTACHMENTS [{}/{}] ──",
                            self.selected_attachment + 1,
                            self.detail_attachments.len()
                        );
                        lines.push(Line::from(Span::styled(
                            indicator,
                            Style::default()
                                .fg(theme::MODE_INDICATOR)
                                .add_modifier(Modifier::BOLD),
                        )));
                    }
                    DetailMode::Links => {
                        let indicator = format!(
                            "── LINKS [{}/{}] ──",
                            if self.detail_links.is_empty() {
                                0
                            } else {
                                self.selected_link + 1
                            },
                            self.detail_links.len()
                        );
                        lines.push(Line::from(Span::styled(
                            indicator,
                            Style::default()
                                .fg(theme::MODE_INDICATOR)
                                .add_modifier(Modifier::BOLD),
                        )));
                    }
                }

                let show_image =
                    self.detail_mode == DetailMode::Attachments && self.image_preview.is_some();

                if !show_image {
                    let text_body = content.text_body.clone();
                    for (line_idx, line_text) in text_body.lines().enumerate() {
                        lines.push(render_body_line(
                            line_text,
                            line_idx,
                            self.detail_mode,
                            &self.detail_links,
                            self.selected_link,
                        ));
                    }
                }
            }

            if self.detail_mode == DetailMode::Attachments && self.image_preview.is_some() {
                use unicode_width::UnicodeWidthStr;
                let width = body_inner.width.max(1) as usize;
                let header_height: u16 = lines
                    .iter()
                    .map(|line| {
                        let w: usize = line
                            .spans
                            .iter()
                            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                            .sum();
                        w.max(1).div_ceil(width) as u16
                    })
                    .sum::<u16>()
                    .min(body_inner.height.saturating_sub(4));
                let [header_area, image_area] =
                    Layout::vertical([Constraint::Length(header_height), Constraint::Min(4)])
                        .areas(body_inner);

                let header_para = Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .style(Style::default().bg(theme::BG_HEADER));
                frame.render_widget(header_para, header_area);

                if let Some(proto) = &mut self.image_preview {
                    frame.render_stateful_widget(StatefulImage::default(), image_area, proto);
                }
            } else {
                let paragraph = Paragraph::new(lines)
                    .scroll((self.detail_scroll, 0))
                    .wrap(Wrap { trim: false })
                    .style(Style::default().bg(theme::BG_HEADER));

                frame.render_widget(paragraph, body_inner);

                if let Some(content) = &self.detail {
                    let body_text = if self.show_raw_headers {
                        &content.raw_headers
                    } else {
                        &content.text_body
                    };
                    let total_lines = body_text.lines().count() + 6;
                    if total_lines > body_inner.height as usize {
                        let mut scrollbar_state =
                            ScrollbarState::new(total_lines).position(self.detail_scroll as usize);
                        frame.render_stateful_widget(
                            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                                .style(Style::default().fg(theme::FG_DIM)),
                            body_inner,
                            &mut scrollbar_state,
                        );
                    }
                }

                if let Some(preview_area) = link_preview_area
                    && let Some(link) = self.detail_links.get(self.selected_link)
                {
                    let sep = "─".repeat(preview_area.width as usize);
                    let preview_lines = vec![
                        Line::from(Span::styled(sep, Style::default().fg(theme::FG_DIM))),
                        Line::from(Span::styled(link.url.clone(), theme::style_link())),
                    ];
                    let preview = Paragraph::new(preview_lines)
                        .wrap(Wrap { trim: false })
                        .style(Style::default().bg(theme::BG_HEADER));
                    frame.render_widget(preview, preview_area);
                }
            }
        }
    }

    fn render_search(&mut self, frame: &mut Frame, area: Rect) {
        let [search_area, results_area] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).areas(area);

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
            let p = Paragraph::new(msg).style(Style::default().fg(theme::FG_DIM).bg(theme::BG));
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
                let mut scrollbar_state =
                    ScrollbarState::new(self.search_results.len()).position(self.search_selected);
                frame.render_stateful_widget(
                    Scrollbar::new(ScrollbarOrientation::VerticalRight)
                        .style(Style::default().fg(theme::FG_DIM)),
                    results_area,
                    &mut scrollbar_state,
                );
            }
        }
    }

    fn render_folder_select(&mut self, frame: &mut Frame, area: Rect) {
        let title_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::SENDER_COLOR))
            .title(" Folders ")
            .title_style(Style::default().fg(theme::SENDER_COLOR))
            .style(Style::default().bg(theme::BG));
        let inner = title_block.inner(area);
        frame.render_widget(title_block, area);

        let visible_height = inner.height as usize;

        // Adjust scroll
        if self.folder_tree_selected < self.folder_tree_scroll {
            self.folder_tree_scroll = self.folder_tree_selected;
        } else if self.folder_tree_selected >= self.folder_tree_scroll + visible_height {
            self.folder_tree_scroll = self.folder_tree_selected - visible_height + 1;
        }

        for (vi, row_idx) in (self.folder_tree_scroll..self.folder_tree.len())
            .enumerate()
            .take(visible_height)
        {
            let y = inner.y + vi as u16;
            if y >= inner.y + inner.height {
                break;
            }
            let row_area = Rect::new(inner.x, y, inner.width, 1);
            let is_selected = row_idx == self.folder_tree_selected;
            let bg = if is_selected {
                theme::BG_SELECTED
            } else {
                theme::BG
            };

            let row = &self.folder_tree[row_idx];
            match row {
                FolderTreeRow::Account { name } => {
                    let is_expanded = self.folder_tree_expanded.contains(name);
                    let arrow = if is_expanded { "▼ " } else { "▶ " };
                    let is_current = *name == self.current_account;
                    let display = if is_current {
                        format!("{}{}  ●", arrow, name)
                    } else {
                        format!("{}{}", arrow, name)
                    };
                    let style = Style::default()
                        .fg(theme::SENDER_COLOR)
                        .bg(bg)
                        .add_modifier(Modifier::BOLD);
                    frame.render_widget(Paragraph::new(display).style(style), row_area);
                }
                FolderTreeRow::Folder {
                    account_name,
                    folder,
                } => {
                    let is_current =
                        *account_name == self.current_account && folder.name == self.current_folder;
                    let marker = if is_current { "  > " } else { "    " };
                    let display = format!("{}{}", marker, folder.name);
                    let style = if is_current {
                        Style::default()
                            .fg(theme::UNREAD_MARKER)
                            .bg(bg)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme::FG_TEXT).bg(bg)
                    };
                    frame.render_widget(Paragraph::new(display).style(style), row_area);
                }
                FolderTreeRow::Loading => {
                    let style = Style::default().fg(theme::FG_DIM).bg(bg);
                    frame.render_widget(
                        Paragraph::new("    Loading folders...").style(style),
                        row_area,
                    );
                }
            }
        }

        if self.folder_tree.len() > visible_height {
            let mut scrollbar_state =
                ScrollbarState::new(self.folder_tree.len()).position(self.folder_tree_selected);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .style(Style::default().fg(theme::FG_DIM)),
                inner,
                &mut scrollbar_state,
            );
        }
    }

    fn render_status_bar(&self, frame: &mut Frame, area: Rect) {
        let (keys, info) = match self.view {
            ViewMode::FolderSelect => (
                vec![
                    ("↑↓", "navigate"),
                    ("Tab/Enter", "expand"),
                    ("Enter", "select"),
                    ("Esc", "back"),
                    ("q", "quit"),
                ],
                format!(" {}/{}", self.current_account, self.current_folder),
            ),
            ViewMode::List => {
                let mut keys = vec![("↑↓", "navigate"), ("Enter", "open")];
                if self.thread_mode {
                    keys.push(("Space", "expand"));
                    keys.push(("Tab", "next thread"));
                    keys.push(("t", "flat"));
                } else {
                    keys.push(("t", "threaded"));
                }
                keys.push(("F", "folders"));
                keys.push(("/", "search"));
                keys.push(("q", "quit"));

                let info = if self.thread_mode {
                    let thread_count = self.threaded_view.threads.len();
                    format!(
                        " {}/{}  {} threads, {} emails | {}",
                        self.current_account,
                        self.current_folder,
                        thread_count,
                        self.emails.len(),
                        if self.status_msg.is_empty() {
                            "jamail"
                        } else {
                            &self.status_msg
                        }
                    )
                } else {
                    format!(
                        " {}/{}  {} emails | {}",
                        self.current_account,
                        self.current_folder,
                        self.emails.len(),
                        if self.status_msg.is_empty() {
                            "jamail"
                        } else {
                            &self.status_msg
                        }
                    )
                };
                (keys, info)
            }
            ViewMode::Detail => {
                let esc_hint = if self.detail_mode != DetailMode::Text {
                    "text"
                } else {
                    "back"
                };
                let mut keys: Vec<(&str, &str)> = vec![("Esc", esc_hint)];
                match self.detail_mode {
                    DetailMode::Text => {
                        keys.push(("↑↓", "scroll"));
                        keys.push(("←→", "prev/next"));
                    }
                    DetailMode::Attachments => {
                        keys.push(("↑↓←→", "select"));
                        keys.push(("s", "save"));
                    }
                    DetailMode::Links => {
                        keys.push(("↑↓←→", "select"));
                        keys.push(("Enter", "open"));
                    }
                }
                if self.detail_thread_info().is_some() {
                    keys.push(("n/p", "thread"));
                }
                keys.push(("/", "mode"));
                keys.push(("h", "headers"));
                keys.push(("v", "browser"));
                keys.push(("q", "quit"));
                let info = if self.show_raw_headers {
                    " Raw headers".to_string()
                } else {
                    match self.detail_mode {
                        DetailMode::Text => " Text".to_string(),
                        DetailMode::Attachments => " Attachments".to_string(),
                        DetailMode::Links => " Links".to_string(),
                    }
                };
                (keys, info)
            }
            ViewMode::Search => (
                vec![
                    ("Esc", "back"),
                    ("Enter", "search/open"),
                    ("↑↓", "navigate"),
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

        let keys_width: usize = spans.iter().map(|s| s.content.len()).sum();
        let padding = (area.width as usize).saturating_sub(keys_width + info.len());
        spans.push(Span::styled(" ".repeat(padding), theme::style_status_bar()));
        spans.push(Span::styled(info, theme::style_status_bar()));

        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

/// Get the display name for a folder (last path component).
fn folder_display_name(folder: &str) -> String {
    // Handle common delimiters: / and .
    if let Some(pos) = folder.rfind('/') {
        folder[pos + 1..].to_string()
    } else if let Some(pos) = folder.rfind('.') {
        folder[pos + 1..].to_string()
    } else {
        folder.to_string()
    }
}

/// Pad a string with trailing spaces to reach exactly `target` display cells.
fn pad_to_display_width(s: &str, target: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    let current = s.width();
    if current >= target {
        s.to_string()
    } else {
        format!("{}{}", s, " ".repeat(target - current))
    }
}

fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn dedup_filename(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    if !path.exists() {
        return path;
    }
    let stem = std::path::Path::new(name)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let ext = std::path::Path::new(name)
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    for i in 1..1000 {
        let candidate = dir.join(format!("{}({}){}", stem, i, ext));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{}.dup", name))
}

fn extract_links(text: &str) -> Vec<LinkInfo> {
    let re_anchor =
        Regex::new(r#"(?i)<a\s[^>]*?href\s*=\s*["']([^"']+)["'][^>]*>(.*?)</a>"#).unwrap();
    let re_url =
        Regex::new(r"https?://[^\s<>\[\]()\x{0022}']+[^\s<>\[\]()\x{0022}'.,;:!?\-]").unwrap();
    let re_www = Regex::new(
        r"\bwww\.[a-zA-Z0-9][-a-zA-Z0-9]*(?:\.[a-zA-Z0-9][-a-zA-Z0-9]*)+(?:/[^\s<>\[\]()\x{0022}']*[^\s<>\[\]()\x{0022}'.,;:!?\-])?"
    ).unwrap();
    let re_bare = Regex::new(
        r"\b(?:[a-zA-Z0-9](?:[-a-zA-Z0-9]*[a-zA-Z0-9])?\.)+(?:com|org|net|edu|gov|io|co|me|hu|de|uk|fr|nl|it|es|pl|se|no|fi|dk|at|ch|be|cz|sk|info|dev|app|xyz|ai|cc|tv|ru|jp|br|au|ca)\b(?:/[^\s<>\[\]()\x{0022}']*[^\s<>\[\]()\x{0022}'.,;:!?\-])?"
    ).unwrap();

    let overlaps = |start: usize, end: usize, ranges: &[(usize, usize)]| -> bool {
        ranges.iter().any(|(rs, re)| start < *re && end > *rs)
    };

    let mut links = Vec::new();
    for (line_idx, line_text) in text.lines().enumerate() {
        let mut used_ranges: Vec<(usize, usize)> = Vec::new();

        for caps in re_anchor.captures_iter(line_text) {
            let full_match = caps.get(0).unwrap();
            let url_raw = caps[1].to_string();
            let link_text = caps[2].to_string();
            if url_raw.starts_with("mailto:")
                || url_raw.starts_with("tel:")
                || url_raw.starts_with("javascript:")
            {
                continue;
            }
            let url = ensure_protocol(&url_raw);
            let display = format!("{} [{}]", link_text.trim(), url);
            let start = full_match.start();
            let end = full_match.end();
            links.push(LinkInfo {
                url,
                display_text: Some(display),
                line: line_idx,
                col_start: start,
                col_end: end,
            });
            used_ranges.push((start, end));
        }

        for m in re_url.find_iter(line_text) {
            if !overlaps(m.start(), m.end(), &used_ranges) {
                links.push(LinkInfo {
                    url: m.as_str().to_string(),
                    display_text: None,
                    line: line_idx,
                    col_start: m.start(),
                    col_end: m.end(),
                });
                used_ranges.push((m.start(), m.end()));
            }
        }

        for m in re_www.find_iter(line_text) {
            if !overlaps(m.start(), m.end(), &used_ranges) {
                links.push(LinkInfo {
                    url: format!("https://{}", m.as_str()),
                    display_text: None,
                    line: line_idx,
                    col_start: m.start(),
                    col_end: m.end(),
                });
                used_ranges.push((m.start(), m.end()));
            }
        }

        for m in re_bare.find_iter(line_text) {
            if !overlaps(m.start(), m.end(), &used_ranges) {
                links.push(LinkInfo {
                    url: format!("https://{}", m.as_str()),
                    display_text: None,
                    line: line_idx,
                    col_start: m.start(),
                    col_end: m.end(),
                });
                used_ranges.push((m.start(), m.end()));
            }
        }
    }

    links.sort_by_key(|l| (l.line, l.col_start));
    links
}

fn ensure_protocol(url: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else if url.starts_with("//") {
        format!("https:{}", url)
    } else {
        format!("https://{}", url)
    }
}

fn render_body_line<'a>(
    line_text: &str,
    line_idx: usize,
    mode: DetailMode,
    links: &[LinkInfo],
    selected_link: usize,
) -> Line<'a> {
    let line_links: Vec<(usize, &LinkInfo)> = links
        .iter()
        .enumerate()
        .filter(|(_, l)| l.line == line_idx)
        .collect();

    if line_links.is_empty() {
        return Line::from(Span::styled(
            line_text.to_string(),
            theme::style_detail_body(),
        ));
    }

    let mut spans = Vec::new();
    let mut pos = 0;
    for (link_global_idx, link) in &line_links {
        if link.col_start > pos {
            let before = &line_text[pos..link.col_start.min(line_text.len())];
            spans.push(Span::styled(before.to_string(), theme::style_detail_body()));
        }
        let display = if let Some(dt) = &link.display_text {
            dt.clone()
        } else {
            line_text[link.col_start.min(line_text.len())..link.col_end.min(line_text.len())]
                .to_string()
        };
        let style = if mode == DetailMode::Links && *link_global_idx == selected_link {
            theme::style_link_selected()
        } else {
            theme::style_link()
        };
        spans.push(Span::styled(display, style));
        pos = link.col_end.min(line_text.len());
    }
    if pos < line_text.len() {
        spans.push(Span::styled(
            line_text[pos..].to_string(),
            theme::style_detail_body(),
        ));
    }
    Line::from(spans)
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
