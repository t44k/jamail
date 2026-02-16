use crate::db::{AttachmentMeta, MailDb};
use crate::mail::{
    Email, EmailContent, FolderInfo, normalize_subject, open_in_browser, relative_time,
};
use crate::smtp::ComposeAttachment;
use crate::theme;
use crate::thread::{DisplayRow, ThreadedView, build_threads, rebuild_rows};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
    },
};
use ratatui_image::{StatefulImage, picker::Picker, protocol::StatefulProtocol};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

const LIST_PEEK_WIDTH: u16 = 12;
const SPINNER_FRAMES: &[char] = &['⣾', '⣽', '⣻', '⢿', '⡿', '⣟', '⣯', '⣷'];

#[derive(Clone, PartialEq, Eq)]
pub enum ViewMode {
    FolderSelect,
    List,
    Detail,
    Search,
    Compose,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ComposeField {
    From,
    To,
    Cc,
    Bcc,
    Subject,
    Body,
    FileBrowser,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ComposeMode {
    New,
    Reply,
    Forward,
}

pub struct FileBrowserEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
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
    GlobalInbox,
    VirtualFolder {
        label: String,
    },
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
    pub status_sticky: bool,
    pub should_quit: bool,
    pub show_help: bool,
    pub help_scroll: usize,
    pub spinner_active: bool,
    pub spinner_tick: usize,
    pub send_result_rx: Option<std::sync::mpsc::Receiver<Result<Option<i64>, String>>>,
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
    pub text_preview: Option<String>,
    pub text_preview_id: Option<i64>,

    // Account/folder context
    pub accounts: Vec<(String, crate::config::JamailAccount)>,
    pub current_account: String,
    pub current_folder: String,
    pub is_global_inbox: bool,
    /// When viewing virtual folders ("Drafts" or "Sent")
    pub virtual_folder: Option<String>,
    /// Cached local messages for drafts/sent view
    pub local_messages: Vec<crate::db::LocalMessage>,

    // Folder selection screen
    pub folder_tree: Vec<FolderTreeRow>,
    pub folder_tree_selected: usize,
    pub folder_tree_expanded: HashSet<String>,
    pub folder_tree_scroll: usize,
    pub account_folders: HashMap<String, Vec<FolderInfo>>,

    // Compose state
    pub compose_mode: ComposeMode,
    pub compose_field: ComposeField,
    pub compose_from: String,
    pub compose_senders: Vec<String>,
    pub compose_sender_index: usize,
    pub compose_to: String,
    pub compose_cc: String,
    pub compose_bcc: String,
    pub compose_subject: String,
    pub compose_body: Vec<String>,
    pub compose_cursor_row: usize,
    pub compose_cursor_col: usize,
    pub compose_body_scroll: usize,
    pub compose_attachments: Vec<ComposeAttachment>,
    pub compose_reply_message_id: Option<String>,
    pub compose_reply_references: Option<String>,
    pub compose_forward_email_id: Option<i64>,
    pub compose_previous_view: ViewMode,
    pub compose_draft_id: Option<i64>,

    // Autocomplete
    pub compose_known_addresses: Vec<String>,
    pub compose_suggestions: Vec<String>,
    pub compose_suggestion_selected: usize,
    pub compose_show_suggestions: bool,

    // File browser
    pub filebrowser_path: PathBuf,
    pub filebrowser_entries: Vec<FileBrowserEntry>,
    pub filebrowser_selected: usize,
    pub filebrowser_scroll: usize,
    pub filebrowser_show_hidden: bool,
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
            status_sticky: false,
            should_quit: false,
            show_help: false,
            help_scroll: 0,
            spinner_active: false,
            spinner_tick: 0,
            send_result_rx: None,
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
            text_preview: None,
            text_preview_id: None,
            accounts,
            current_account,
            current_folder,
            is_global_inbox: false,
            virtual_folder: None,
            local_messages: Vec::new(),
            folder_tree: Vec::new(),
            folder_tree_selected: 0,
            folder_tree_expanded,
            folder_tree_scroll: 0,
            account_folders: HashMap::new(),

            compose_mode: ComposeMode::New,
            compose_field: ComposeField::To,
            compose_from: String::new(),
            compose_senders: Vec::new(),
            compose_sender_index: 0,
            compose_to: String::new(),
            compose_cc: String::new(),
            compose_bcc: String::new(),
            compose_subject: String::new(),
            compose_body: vec![String::new()],
            compose_cursor_row: 0,
            compose_cursor_col: 0,
            compose_body_scroll: 0,
            compose_attachments: Vec::new(),
            compose_reply_message_id: None,
            compose_reply_references: None,
            compose_forward_email_id: None,
            compose_previous_view: ViewMode::List,
            compose_draft_id: None,

            compose_known_addresses: Vec::new(),
            compose_suggestions: Vec::new(),
            compose_suggestion_selected: 0,
            compose_show_suggestions: false,

            filebrowser_path: dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")),
            filebrowser_entries: Vec::new(),
            filebrowser_selected: 0,
            filebrowser_scroll: 0,
            filebrowser_show_hidden: false,
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
                            let idx =
                                self.threaded_view.threads[thread_idx].email_indices[email_idx];
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
                    if thread
                        .email_indices
                        .iter()
                        .any(|&idx| self.emails[idx].id == id)
                    {
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

        let result = if self.is_global_inbox {
            db.get_global_inbox()
        } else {
            db.get_email_list(&self.current_account, &self.current_folder)
        };
        if let Ok(emails) = result {
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
        }
    }

    pub fn load_virtual_folder(&mut self, db: &MailDb, folder: &str) {
        self.virtual_folder = Some(folder.to_string());
        self.is_global_inbox = false;
        let result = match folder {
            "Drafts" => db.get_drafts(&self.current_account),
            "Sent" => db.get_sent(&self.current_account),
            _ => return,
        };
        match result {
            Ok(msgs) => {
                self.local_messages = msgs;
                // Convert to Email entries for display in the list
                self.emails = self
                    .local_messages
                    .iter()
                    .map(|m| {
                        let date = chrono::DateTime::parse_from_rfc3339(&m.created_at)
                            .map(|d| d.with_timezone(&chrono::Local))
                            .unwrap_or_else(|_| chrono::Local::now());
                        Email {
                            id: -m.id, // Negative to distinguish from real emails
                            uid: 0,
                            account: m.account.clone(),
                            from: m.from_addr.clone(),
                            subject: if m.subject.is_empty() {
                                "(no subject)".to_string()
                            } else {
                                m.subject.clone()
                            },
                            date,
                            is_unread: m.status == "draft",
                            preview: truncate_str(&m.to_addr, 80),
                            message_id: String::new(),
                            in_reply_to: m.in_reply_to.clone(),
                            references: m.refs.clone(),
                            has_attachments: false,
                        }
                    })
                    .collect();
                self.threaded_view.threads = build_threads(&self.emails);
                rebuild_rows(&mut self.threaded_view);
                self.selected = 0;
                self.list_scroll_offset = 0;
            }
            Err(_) => {
                self.local_messages.clear();
                self.emails.clear();
            }
        }
    }

    pub fn resume_draft(&mut self, db: &MailDb, draft_idx: usize) {
        if draft_idx >= self.local_messages.len() {
            return;
        }
        let draft = self.local_messages[draft_idx].clone();
        self.compose_mode = ComposeMode::New;
        self.compose_field = ComposeField::Body;
        self.compose_from = draft.from_addr;
        self.compose_senders = self.build_sender_list();
        self.compose_sender_index = self
            .compose_senders
            .iter()
            .position(|s| s == &self.compose_from)
            .unwrap_or(0);
        self.compose_to = draft.to_addr;
        self.compose_cc = draft.cc;
        self.compose_bcc = draft.bcc;
        self.compose_subject = draft.subject;
        self.compose_body = if draft.body.is_empty() {
            vec![String::new()]
        } else {
            draft.body.lines().map(|l| l.to_string()).collect()
        };
        self.compose_cursor_row = 0;
        self.compose_cursor_col = 0;
        self.compose_body_scroll = 0;
        self.compose_attachments.clear();
        self.compose_reply_message_id = if draft.in_reply_to.is_empty() {
            None
        } else {
            Some(draft.in_reply_to)
        };
        self.compose_reply_references = if draft.refs.is_empty() {
            None
        } else {
            Some(draft.refs)
        };
        self.compose_forward_email_id = None;
        self.compose_draft_id = Some(draft.id);
        self.compose_previous_view = self.view.clone();
        self.view = ViewMode::Compose;

        // Load known addresses for autocomplete
        if let Ok(addrs) = db.get_known_addresses() {
            self.compose_known_addresses = addrs;
        }
    }

    pub fn save_compose_as_draft(&mut self, db: &MailDb) {
        let body = self.compose_body.join("\n");
        let in_reply_to = self.compose_reply_message_id.as_deref().unwrap_or("");
        let refs = self.compose_reply_references.as_deref().unwrap_or("");

        let result = if let Some(draft_id) = self.compose_draft_id {
            db.update_draft(
                draft_id,
                &self.compose_from,
                &self.compose_to,
                &self.compose_cc,
                &self.compose_bcc,
                &self.compose_subject,
                &body,
            )
            .map(|_| draft_id)
        } else {
            db.save_draft(
                &self.current_account,
                &self.compose_from,
                &self.compose_to,
                &self.compose_cc,
                &self.compose_bcc,
                &self.compose_subject,
                &body,
                in_reply_to,
                refs,
            )
        };

        match result {
            Ok(id) => {
                self.compose_draft_id = Some(id);
                self.status_msg = "Draft saved".to_string();
                self.status_sticky = true;
            }
            Err(e) => {
                self.status_msg = format!("Failed to save draft: {}", e);
                self.status_sticky = true;
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
        self.text_preview = None;
        self.text_preview_id = None;
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
                        let idx = self.threaded_view.threads[*thread_idx].email_indices[*email_idx];
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
                    if thread
                        .email_indices
                        .iter()
                        .any(|&idx| self.emails[idx].id == id)
                    {
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
                    let idx = self.threaded_view.threads[*thread_idx].email_indices[*email_idx];
                    self.emails[idx].id == id
                }
                DisplayRow::ThreadSummary { thread_idx } => {
                    let thread = &self.threaded_view.threads[*thread_idx];
                    thread.message_count == 1 && self.emails[thread.email_indices[0]].id == id
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

    pub fn open_attachment(&mut self, db: &MailDb) {
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

        let tmp_dir = PathBuf::from(format!("/tmp/jamail_open_{}", att.id));
        if let Err(e) = std::fs::create_dir_all(&tmp_dir) {
            self.status_msg = format!("Failed to create temp dir: {}", e);
            return;
        }
        let path = tmp_dir.join(&att.filename);
        if let Err(e) = std::fs::write(&path, &data) {
            self.status_msg = format!("Failed to write temp file: {}", e);
            return;
        }

        match std::process::Command::new("xdg-open")
            .arg(&path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(_) => {
                self.status_msg = format!("Opening {}", att.filename);
            }
            Err(e) => {
                self.status_msg = format!("Failed to open: {}", e);
            }
        }
    }

    pub fn update_image_preview(&mut self, db: &MailDb) {
        if self.detail_mode != DetailMode::Attachments || self.detail_attachments.is_empty() {
            self.image_preview = None;
            self.image_preview_id = None;
            self.text_preview = None;
            self.text_preview_id = None;
            return;
        }

        let att = &self.detail_attachments[self.selected_attachment];
        let att_id = att.id;

        // Skip if already previewing this attachment
        if self.image_preview_id == Some(att_id) || self.text_preview_id == Some(att_id) {
            return;
        }

        // Clear both previews
        self.image_preview = None;
        self.image_preview_id = None;
        self.text_preview = None;
        self.text_preview_id = None;

        let mime_type = att.mime_type.clone();
        let filename = att.filename.clone();

        let data = match db.get_attachment_data(att_id) {
            Ok(Some(d)) => d,
            _ => return,
        };

        // Try image preview first
        if mime_type.starts_with("image/")
            && let Some(picker) = &self.image_picker
            && let Ok(img) = image::load_from_memory(&data)
        {
            self.image_preview = Some(picker.new_resize_protocol(img));
            self.image_preview_id = Some(att_id);
            return;
        }

        // Try text-based preview for non-image types
        if let Some(text) = generate_text_preview(&data, &mime_type, &filename) {
            self.text_preview = Some(text);
            self.text_preview_id = Some(att_id);
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
        // Virtual folders at the top
        self.folder_tree.push(FolderTreeRow::VirtualFolder {
            label: "Drafts".to_string(),
        });
        self.folder_tree.push(FolderTreeRow::VirtualFolder {
            label: "Sent".to_string(),
        });
        // Show "All Inboxes" when multiple accounts exist
        if self.accounts.len() > 1 {
            self.folder_tree.push(FolderTreeRow::GlobalInbox);
        }
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
                FolderTreeRow::GlobalInbox | FolderTreeRow::VirtualFolder { .. } | FolderTreeRow::Folder { .. } | FolderTreeRow::Loading => {}
            }
        }
    }

    /// Returns Some(("*global*", "INBOX")) for global inbox,
    /// Some(("*virtual*", "Drafts"|"Sent")) for virtual folders,
    /// Some((account, folder)) for a specific folder, or None.
    pub fn folder_tree_select(&mut self) -> Option<(String, String)> {
        if let Some(row) = self.folder_tree.get(self.folder_tree_selected) {
            match row {
                FolderTreeRow::GlobalInbox => {
                    Some(("*global*".to_string(), "INBOX".to_string()))
                }
                FolderTreeRow::VirtualFolder { label } => {
                    Some(("*virtual*".to_string(), label.clone()))
                }
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
        if self.is_global_inbox {
            "All".to_string()
        } else if let Some(ref vf) = self.virtual_folder {
            vf.clone()
        } else {
            folder_display_name(&self.current_folder)
        }
    }

    /// Get the color for an account (from config or default palette).
    fn account_color(&self, account_name: &str) -> Color {
        // Check if account has an explicit color in config
        if let Some((_, acc)) = self.accounts.iter().find(|(n, _)| n == account_name)
            && let Some(ref hex) = acc.color
            && let Some(c) = parse_hex_color(hex)
        {
            return c;
        }
        // Fall back to palette based on account index
        let idx = self
            .accounts
            .iter()
            .position(|(n, _)| n == account_name)
            .unwrap_or(0);
        theme::ACCOUNT_COLORS[idx % theme::ACCOUNT_COLORS.len()]
    }

    // ── Compose ────────────────────────────────────────────────────

    fn build_sender_list(&self) -> Vec<String> {
        let account = self
            .accounts
            .iter()
            .find(|(n, _)| *n == self.current_account);
        if let Some((_, acc)) = account {
            if let Some(ref senders) = acc.senders
                && !senders.is_empty()
            {
                return senders.clone();
            }
            let addr = if let Some(ref name) = acc.display_name {
                format!("{} <{}>", name, acc.email)
            } else {
                acc.email.clone()
            };
            vec![addr]
        } else {
            Vec::new()
        }
    }

    pub fn cycle_sender_forward(&mut self) {
        if self.compose_senders.len() <= 1 {
            return;
        }
        self.compose_sender_index = (self.compose_sender_index + 1) % self.compose_senders.len();
        self.compose_from = self.compose_senders[self.compose_sender_index].clone();
    }

    pub fn cycle_sender_backward(&mut self) {
        if self.compose_senders.len() <= 1 {
            return;
        }
        if self.compose_sender_index == 0 {
            self.compose_sender_index = self.compose_senders.len() - 1;
        } else {
            self.compose_sender_index -= 1;
        }
        self.compose_from = self.compose_senders[self.compose_sender_index].clone();
    }

    fn clear_compose(&mut self) {
        self.compose_to.clear();
        self.compose_cc.clear();
        self.compose_bcc.clear();
        self.compose_subject.clear();
        self.compose_body = vec![String::new()];
        self.compose_cursor_row = 0;
        self.compose_cursor_col = 0;
        self.compose_body_scroll = 0;
        self.compose_attachments.clear();
        self.compose_reply_message_id = None;
        self.compose_reply_references = None;
        self.cleanup_forward_temps();
        self.compose_forward_email_id = None;
        self.compose_draft_id = None;
        self.compose_suggestions.clear();
        self.compose_suggestion_selected = 0;
        self.compose_show_suggestions = false;
    }

    pub fn enter_compose_new(&mut self, db: &MailDb) {
        self.compose_previous_view = self.view.clone();
        self.clear_compose();
        self.compose_mode = ComposeMode::New;
        self.compose_senders = self.build_sender_list();
        self.compose_sender_index = 0;
        self.compose_from = self.compose_senders.first().cloned().unwrap_or_default();
        self.compose_field = ComposeField::To;
        self.compose_known_addresses = db.get_known_addresses().unwrap_or_default();
        self.view = ViewMode::Compose;
    }

    pub fn enter_reply(&mut self, db: &MailDb) {
        let detail = match self.detail.clone() {
            Some(d) => d,
            None => return,
        };
        let email_id = match self.detail_id {
            Some(id) => id,
            None => return,
        };
        let email = self.emails.iter().find(|e| e.id == email_id).cloned();

        self.compose_previous_view = self.view.clone();
        self.clear_compose();
        self.compose_mode = ComposeMode::Reply;
        self.compose_senders = self.build_sender_list();
        self.compose_sender_index = 0;
        self.compose_from = self.compose_senders.first().cloned().unwrap_or_default();
        self.compose_to = detail.from.clone();

        let normalized = normalize_subject(&detail.subject);
        self.compose_subject = if normalized == detail.subject {
            format!("Re: {}", detail.subject)
        } else {
            format!("Re: {}", normalized)
        };

        // Build quoted body
        let date_str = detail.date.format("%a, %d %b %Y %H:%M").to_string();
        let mut body_lines = vec![
            String::new(),
            String::new(),
            format!("On {}, {} wrote:", date_str, detail.from),
        ];
        for line in detail.text_body.lines() {
            body_lines.push(format!("> {}", line));
        }
        self.compose_body = body_lines;
        self.compose_cursor_row = 0;
        self.compose_cursor_col = 0;

        // Threading headers
        if let Some(ref e) = email
            && !e.message_id.is_empty()
        {
            self.compose_reply_message_id = Some(e.message_id.clone());
            // Build References chain
            let refs = if !e.references.is_empty() {
                format!("{} {}", e.references, e.message_id)
            } else {
                e.message_id.clone()
            };
            self.compose_reply_references = Some(refs);
        }

        self.compose_field = ComposeField::Body;
        self.compose_known_addresses = db.get_known_addresses().unwrap_or_default();
        self.view = ViewMode::Compose;
    }

    pub fn enter_forward(&mut self, db: &MailDb) {
        let detail = match self.detail.clone() {
            Some(d) => d,
            None => return,
        };
        let email_id = match self.detail_id {
            Some(id) => id,
            None => return,
        };

        self.compose_previous_view = self.view.clone();
        self.clear_compose();
        self.compose_mode = ComposeMode::Forward;
        self.compose_senders = self.build_sender_list();
        self.compose_sender_index = 0;
        self.compose_from = self.compose_senders.first().cloned().unwrap_or_default();

        let normalized = normalize_subject(&detail.subject);
        self.compose_subject = if normalized == detail.subject {
            format!("Fwd: {}", detail.subject)
        } else {
            format!("Fwd: {}", normalized)
        };

        // Build forwarded message body
        let date_str = detail.date.format("%a, %d %b %Y %H:%M:%S").to_string();
        let mut body_lines = vec![
            String::new(),
            String::new(),
            "---------- Forwarded message ----------".to_string(),
            format!("From: {}", detail.from),
            format!("Date: {}", date_str),
            format!("Subject: {}", detail.subject),
            format!("To: {}", detail.to),
            String::new(),
        ];
        for line_str in detail.text_body.lines() {
            body_lines.push(line_str.to_string());
        }
        self.compose_body = body_lines;
        self.compose_cursor_row = 0;
        self.compose_cursor_col = 0;

        // Auto-attach original attachments to temp files
        let metas = db.get_attachments_meta(email_id).unwrap_or_default();
        if !metas.is_empty() {
            let tmp_dir = PathBuf::from(format!("/tmp/jamail_fwd_{}", email_id));
            let _ = std::fs::create_dir_all(&tmp_dir);
            for meta in &metas {
                if let Ok(Some(data)) = db.get_attachment_data(meta.id) {
                    let path = tmp_dir.join(&meta.filename);
                    if std::fs::write(&path, &data).is_ok() {
                        self.compose_attachments.push(ComposeAttachment {
                            path: path.clone(),
                            filename: meta.filename.clone(),
                            size: data.len() as u64,
                        });
                    }
                }
            }
            self.compose_forward_email_id = Some(email_id);
        }

        self.compose_field = ComposeField::To;
        self.compose_known_addresses = db.get_known_addresses().unwrap_or_default();
        self.view = ViewMode::Compose;
    }

    pub fn cancel_compose(&mut self) {
        self.cleanup_forward_temps();
        self.view = self.compose_previous_view.clone();
    }

    pub fn cleanup_forward_temps(&mut self) {
        if let Some(id) = self.compose_forward_email_id.take() {
            let tmp_dir = PathBuf::from(format!("/tmp/jamail_fwd_{}", id));
            let _ = std::fs::remove_dir_all(&tmp_dir);
        }
    }

    // ── Compose body editor ──────────────────────────────────────

    fn char_to_byte_pos(s: &str, char_idx: usize) -> usize {
        s.char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(s.len())
    }

    pub fn compose_body_insert_char(&mut self, ch: char) {
        if self.compose_cursor_row >= self.compose_body.len() {
            self.compose_body.push(String::new());
            self.compose_cursor_row = self.compose_body.len() - 1;
        }
        let byte_pos = Self::char_to_byte_pos(
            &self.compose_body[self.compose_cursor_row],
            self.compose_cursor_col,
        );
        self.compose_body[self.compose_cursor_row].insert(byte_pos, ch);
        self.compose_cursor_col += 1;
    }

    pub fn compose_body_newline(&mut self) {
        if self.compose_cursor_row >= self.compose_body.len() {
            self.compose_body.push(String::new());
            self.compose_cursor_row = self.compose_body.len() - 1;
        }
        let byte_pos = Self::char_to_byte_pos(
            &self.compose_body[self.compose_cursor_row],
            self.compose_cursor_col,
        );
        let rest = self.compose_body[self.compose_cursor_row][byte_pos..].to_string();
        self.compose_body[self.compose_cursor_row].truncate(byte_pos);
        self.compose_cursor_row += 1;
        self.compose_body.insert(self.compose_cursor_row, rest);
        self.compose_cursor_col = 0;
    }

    pub fn compose_body_backspace(&mut self) {
        if self.compose_cursor_col > 0 {
            let byte_start = Self::char_to_byte_pos(
                &self.compose_body[self.compose_cursor_row],
                self.compose_cursor_col - 1,
            );
            let byte_end = Self::char_to_byte_pos(
                &self.compose_body[self.compose_cursor_row],
                self.compose_cursor_col,
            );
            self.compose_body[self.compose_cursor_row].replace_range(byte_start..byte_end, "");
            self.compose_cursor_col -= 1;
        } else if self.compose_cursor_row > 0 {
            let current_line = self.compose_body.remove(self.compose_cursor_row);
            self.compose_cursor_row -= 1;
            self.compose_cursor_col = self.compose_body[self.compose_cursor_row].chars().count();
            self.compose_body[self.compose_cursor_row].push_str(&current_line);
        }
    }

    pub fn compose_body_delete(&mut self) {
        if self.compose_cursor_row >= self.compose_body.len() {
            return;
        }
        let line_chars = self.compose_body[self.compose_cursor_row].chars().count();
        if self.compose_cursor_col < line_chars {
            let byte_start = Self::char_to_byte_pos(
                &self.compose_body[self.compose_cursor_row],
                self.compose_cursor_col,
            );
            let byte_end = Self::char_to_byte_pos(
                &self.compose_body[self.compose_cursor_row],
                self.compose_cursor_col + 1,
            );
            self.compose_body[self.compose_cursor_row].replace_range(byte_start..byte_end, "");
        } else if self.compose_cursor_row + 1 < self.compose_body.len() {
            let next_line = self.compose_body.remove(self.compose_cursor_row + 1);
            self.compose_body[self.compose_cursor_row].push_str(&next_line);
        }
    }

    pub fn compose_body_left(&mut self) {
        if self.compose_cursor_col > 0 {
            self.compose_cursor_col -= 1;
        } else if self.compose_cursor_row > 0 {
            self.compose_cursor_row -= 1;
            self.compose_cursor_col = self.compose_body[self.compose_cursor_row].chars().count();
        }
    }

    pub fn compose_body_right(&mut self) {
        if self.compose_cursor_row >= self.compose_body.len() {
            return;
        }
        let line_chars = self.compose_body[self.compose_cursor_row].chars().count();
        if self.compose_cursor_col < line_chars {
            self.compose_cursor_col += 1;
        } else if self.compose_cursor_row + 1 < self.compose_body.len() {
            self.compose_cursor_row += 1;
            self.compose_cursor_col = 0;
        }
    }

    pub fn compose_body_up(&mut self) {
        if self.compose_cursor_row > 0 {
            self.compose_cursor_row -= 1;
            let line_chars = self.compose_body[self.compose_cursor_row].chars().count();
            self.compose_cursor_col = self.compose_cursor_col.min(line_chars);
        }
    }

    pub fn compose_body_down(&mut self) {
        if self.compose_cursor_row + 1 < self.compose_body.len() {
            self.compose_cursor_row += 1;
            let line_chars = self.compose_body[self.compose_cursor_row].chars().count();
            self.compose_cursor_col = self.compose_cursor_col.min(line_chars);
        }
    }

    pub fn compose_body_home(&mut self) {
        self.compose_cursor_col = 0;
    }

    pub fn compose_body_end(&mut self) {
        if self.compose_cursor_row < self.compose_body.len() {
            self.compose_cursor_col = self.compose_body[self.compose_cursor_row].chars().count();
        }
    }

    fn compose_ensure_cursor_visible(&mut self, viewport_height: usize) {
        if viewport_height == 0 {
            return;
        }
        if self.compose_cursor_row < self.compose_body_scroll {
            self.compose_body_scroll = self.compose_cursor_row;
        } else if self.compose_cursor_row >= self.compose_body_scroll + viewport_height {
            self.compose_body_scroll = self.compose_cursor_row - viewport_height + 1;
        }
    }

    // ── Autocomplete ─────────────────────────────────────────────

    fn compose_active_address_field(&mut self) -> &mut String {
        match self.compose_field {
            ComposeField::To => &mut self.compose_to,
            ComposeField::Cc => &mut self.compose_cc,
            ComposeField::Bcc => &mut self.compose_bcc,
            _ => &mut self.compose_to, // fallback
        }
    }

    fn compose_active_address_field_ref(&self) -> &String {
        match self.compose_field {
            ComposeField::To => &self.compose_to,
            ComposeField::Cc => &self.compose_cc,
            ComposeField::Bcc => &self.compose_bcc,
            _ => &self.compose_to,
        }
    }

    pub fn compose_update_suggestions(&mut self) {
        if !matches!(
            self.compose_field,
            ComposeField::To | ComposeField::Cc | ComposeField::Bcc
        ) {
            self.compose_show_suggestions = false;
            self.compose_suggestions.clear();
            return;
        }

        let field = self.compose_active_address_field_ref().clone();
        let current_token = field.rsplit(',').next().unwrap_or("").trim().to_lowercase();

        if current_token.len() < 2 {
            self.compose_show_suggestions = false;
            self.compose_suggestions.clear();
            return;
        }

        self.compose_suggestions = self
            .compose_known_addresses
            .iter()
            .filter(|a| a.to_lowercase().contains(&current_token))
            .take(10)
            .cloned()
            .collect();

        self.compose_show_suggestions = !self.compose_suggestions.is_empty();
        self.compose_suggestion_selected = 0;
    }

    pub fn compose_accept_suggestion(&mut self) {
        if !self.compose_show_suggestions || self.compose_suggestions.is_empty() {
            return;
        }
        let suggestion = self.compose_suggestions[self.compose_suggestion_selected].clone();
        let field = self.compose_active_address_field();

        // Replace text after last comma with the suggestion
        if let Some(last_comma) = field.rfind(',') {
            field.truncate(last_comma + 1);
            field.push(' ');
            field.push_str(&suggestion);
        } else {
            *field = suggestion;
        }
        field.push_str(", ");

        self.compose_show_suggestions = false;
        self.compose_suggestions.clear();
    }

    pub fn compose_address_input(&mut self, ch: char) {
        let field = self.compose_active_address_field();
        field.push(ch);
        self.compose_update_suggestions();
    }

    pub fn compose_address_backspace(&mut self) {
        let field = self.compose_active_address_field();
        field.pop();
        self.compose_update_suggestions();
    }

    pub fn compose_next_field(&mut self) {
        self.compose_show_suggestions = false;
        self.compose_field = match self.compose_field {
            ComposeField::From => ComposeField::To,
            ComposeField::To => ComposeField::Cc,
            ComposeField::Cc => ComposeField::Bcc,
            ComposeField::Bcc => ComposeField::Subject,
            ComposeField::Subject => ComposeField::Body,
            ComposeField::Body => ComposeField::Body,
            ComposeField::FileBrowser => ComposeField::FileBrowser,
        };
    }

    pub fn compose_prev_field(&mut self) {
        self.compose_show_suggestions = false;
        self.compose_field = match self.compose_field {
            ComposeField::From => ComposeField::From,
            ComposeField::To => {
                if self.compose_senders.len() > 1 {
                    ComposeField::From
                } else {
                    ComposeField::To
                }
            }
            ComposeField::Cc => ComposeField::To,
            ComposeField::Bcc => ComposeField::Cc,
            ComposeField::Subject => ComposeField::Bcc,
            ComposeField::Body => ComposeField::Subject,
            ComposeField::FileBrowser => ComposeField::Body,
        };
    }

    // ── File browser ─────────────────────────────────────────────

    pub fn open_file_browser(&mut self) {
        self.compose_field = ComposeField::FileBrowser;
        self.filebrowser_selected = 0;
        self.filebrowser_scroll = 0;
        self.refresh_filebrowser();
    }

    pub fn refresh_filebrowser(&mut self) {
        self.filebrowser_entries.clear();
        let entries = match std::fs::read_dir(&self.filebrowser_path) {
            Ok(e) => e,
            Err(_) => return,
        };

        let mut dirs = Vec::new();
        let mut files = Vec::new();

        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !self.filebrowser_show_hidden && name.starts_with('.') {
                continue;
            }
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            let entry = FileBrowserEntry { name, is_dir, size };
            if is_dir {
                dirs.push(entry);
            } else {
                files.push(entry);
            }
        }

        dirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

        self.filebrowser_entries = dirs;
        self.filebrowser_entries.extend(files);
        self.filebrowser_selected = 0;
        self.filebrowser_scroll = 0;
    }

    pub fn filebrowser_up(&mut self) {
        if self.filebrowser_selected > 0 {
            self.filebrowser_selected -= 1;
        }
    }

    pub fn filebrowser_down(&mut self) {
        if self.filebrowser_selected + 1 < self.filebrowser_entries.len() {
            self.filebrowser_selected += 1;
        }
    }

    pub fn filebrowser_enter(&mut self) {
        if let Some(entry) = self.filebrowser_entries.get(self.filebrowser_selected) {
            if entry.is_dir {
                self.filebrowser_path = self.filebrowser_path.join(&entry.name);
                self.refresh_filebrowser();
            } else {
                // Attach the file
                let path = self.filebrowser_path.join(&entry.name);
                let size = entry.size;
                let filename = entry.name.clone();
                self.compose_attachments.push(ComposeAttachment {
                    path,
                    filename,
                    size,
                });
                self.compose_field = ComposeField::Body;
            }
        }
    }

    pub fn filebrowser_parent(&mut self) {
        if let Some(parent) = self.filebrowser_path.parent() {
            self.filebrowser_path = parent.to_path_buf();
            self.refresh_filebrowser();
        }
    }

    pub fn filebrowser_toggle_hidden(&mut self) {
        self.filebrowser_show_hidden = !self.filebrowser_show_hidden;
        self.refresh_filebrowser();
    }

    pub fn compose_remove_last_attachment(&mut self) {
        if let Some(att) = self.compose_attachments.pop() {
            self.status_msg = format!("Removed: {}", att.filename);
        }
    }

    pub fn compose_body_text(&self) -> String {
        self.compose_body.join("\n")
    }

    // ── Rendering ───────────────────────────────────────────────────

    pub fn render(&mut self, frame: &mut Frame) {
        if self.spinner_active {
            self.spinner_tick = self.spinner_tick.wrapping_add(1);
        }
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
            ViewMode::Compose => self.render_compose(frame, content_area),
        }

        self.render_status_bar(frame, status_area);

        if self.show_help {
            self.render_help_overlay(frame, area);
        }
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

        let acct_pip_w = if self.is_global_inbox { 2usize } else { 0 };
        let marker_w = 2usize;
        let sender_w = 20usize.min(width / 4);
        let rel_time_w = 14usize;
        let exact_time_w = 18usize;
        let fixed_w = acct_pip_w + marker_w + sender_w + rel_time_w + exact_time_w + 3;
        let subject_w = if width > fixed_w { width - fixed_w } else { 10 };

        let unread_marker = if email.is_unread { "●" } else { " " };
        let attach_marker = if email.has_attachments { "@" } else { " " };
        let sender = truncate_str(&email.from, sender_w);
        let subject = truncate_str(&email.subject, subject_w);
        let rel_time = relative_time(&email.date);
        let exact_time = email.date.format("%Y-%m-%d %H:%M").to_string();

        let rel_time = format!("{:>width$}", rel_time, width = rel_time_w);
        let exact_time = format!("{:>width$}", exact_time, width = exact_time_w);

        let mut spans = Vec::new();

        // Account color pip for global inbox
        if self.is_global_inbox {
            let color = self.account_color(&email.account);
            spans.push(Span::styled("●", Style::default().fg(color).bg(bg)));
            spans.push(Span::styled(" ", Style::default().bg(bg)));
        }

        spans.extend([
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
        ]);

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

                // Show account badge in global inbox mode
                if self.is_global_inbox
                    && let Some(email) = self.selected_email()
                {
                    let acct = email.account.clone();
                    let color = self.account_color(&acct);
                    lines.push(Line::from(vec![
                        Span::styled("Account: ", theme::style_detail_header_label()),
                        Span::styled(
                            format!(" {} ", acct),
                            Style::default()
                                .fg(Color::Rgb(18, 18, 24))
                                .bg(color)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]));
                }

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
                let show_text_preview = self.detail_mode == DetailMode::Attachments
                    && self.text_preview.is_some()
                    && !show_image;

                if !show_image && !show_text_preview {
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
                if show_text_preview
                    && let Some(preview_text) = &self.text_preview
                {
                    lines.push(Line::from(""));
                    lines.push(Line::from(Span::styled(
                        "── Preview ──",
                        Style::default()
                            .fg(theme::MODE_INDICATOR)
                            .add_modifier(Modifier::BOLD),
                    )));
                    for line_text in preview_text.lines().take(200) {
                        lines.push(Line::from(Span::styled(
                            line_text.to_string(),
                            theme::style_detail_body(),
                        )));
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
                FolderTreeRow::GlobalInbox => {
                    let is_current = self.is_global_inbox;
                    let marker = if is_current { "● " } else { "  " };
                    let display = format!("{}All Inboxes", marker);
                    let style = Style::default()
                        .fg(theme::HELP_TITLE)
                        .bg(bg)
                        .add_modifier(Modifier::BOLD);
                    frame.render_widget(Paragraph::new(display).style(style), row_area);
                }
                FolderTreeRow::VirtualFolder { label } => {
                    let is_current = self.virtual_folder.as_deref() == Some(label.as_str());
                    let marker = if is_current { "● " } else { "  " };
                    let icon = if label == "Drafts" { "✎ " } else { "➤ " };
                    let display = format!("{}{}{}", marker, icon, label);
                    let fg = if label == "Drafts" {
                        theme::MODE_INDICATOR
                    } else {
                        theme::COMPOSE_FIELD_ACTIVE
                    };
                    let style = Style::default().fg(fg).bg(bg);
                    frame.render_widget(Paragraph::new(display).style(style), row_area);
                }
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

    fn render_compose(&mut self, frame: &mut Frame, area: Rect) {
        let compose_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::COMPOSE_BORDER))
            .title(" Compose ")
            .title_style(
                Style::default()
                    .fg(theme::SENDER_COLOR)
                    .add_modifier(Modifier::BOLD),
            )
            .style(Style::default().bg(theme::BG));
        let inner = compose_block.inner(area);
        frame.render_widget(compose_block, area);

        if inner.height < 5 || inner.width < 20 {
            return;
        }

        // File browser mode
        if self.compose_field == ComposeField::FileBrowser {
            self.render_filebrowser(frame, inner);
            return;
        }

        let mut y = inner.y;
        let w = inner.width as usize;
        let label_w = 9; // "Subject: " length

        // From
        if y < inner.y + inner.height {
            let is_from_active = self.compose_field == ComposeField::From;
            let has_multiple = self.compose_senders.len() > 1;
            let label_style = if is_from_active {
                Style::default().fg(theme::COMPOSE_FIELD_ACTIVE)
            } else {
                Style::default().fg(theme::FG_DIM)
            };
            let value_style = if is_from_active {
                Style::default().fg(theme::COMPOSE_CURSOR)
            } else {
                Style::default().fg(theme::FG_DIM)
            };
            let mut spans = vec![
                Span::styled("From:    ", label_style),
                Span::styled(&self.compose_from, value_style),
            ];
            if has_multiple {
                let indicator = format!(
                    " [{}/{}]",
                    self.compose_sender_index + 1,
                    self.compose_senders.len()
                );
                if is_from_active {
                    spans.push(Span::styled(
                        indicator,
                        Style::default().fg(theme::MODE_INDICATOR),
                    ));
                    spans.push(Span::styled(
                        " ←→",
                        Style::default().fg(theme::FG_DIM),
                    ));
                } else {
                    spans.push(Span::styled(indicator, Style::default().fg(theme::FG_DIM)));
                }
            }
            frame.render_widget(
                Paragraph::new(Line::from(spans)).style(Style::default().bg(theme::BG)),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y += 1;
        }

        // To
        if y < inner.y + inner.height {
            let is_active = self.compose_field == ComposeField::To;
            self.render_compose_field(
                frame,
                inner.x,
                y,
                inner.width,
                "To:      ",
                &self.compose_to.clone(),
                is_active,
            );
            y += 1;
        }

        // Cc
        if y < inner.y + inner.height {
            let is_active = self.compose_field == ComposeField::Cc;
            self.render_compose_field(
                frame,
                inner.x,
                y,
                inner.width,
                "Cc:      ",
                &self.compose_cc.clone(),
                is_active,
            );
            y += 1;
        }

        // Bcc
        if y < inner.y + inner.height {
            let is_active = self.compose_field == ComposeField::Bcc;
            self.render_compose_field(
                frame,
                inner.x,
                y,
                inner.width,
                "Bcc:     ",
                &self.compose_bcc.clone(),
                is_active,
            );
            y += 1;
        }

        // Subject
        if y < inner.y + inner.height {
            let is_active = self.compose_field == ComposeField::Subject;
            self.render_compose_field(
                frame,
                inner.x,
                y,
                inner.width,
                "Subject: ",
                &self.compose_subject.clone(),
                is_active,
            );
            y += 1;
        }

        // Attachments line (only if any)
        if !self.compose_attachments.is_empty() && y < inner.y + inner.height {
            let att_str: String = self
                .compose_attachments
                .iter()
                .map(|a| {
                    format!(
                        "{} ({})",
                        a.filename,
                        crate::smtp::format_attachment_size(a.size)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            let line = Line::from(vec![
                Span::styled("Attach:  ", Style::default().fg(theme::ATTACHMENT_COLOR)),
                Span::styled(att_str, Style::default().fg(theme::ATTACHMENT_COLOR)),
            ]);
            frame.render_widget(
                Paragraph::new(line).style(Style::default().bg(theme::BG)),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y += 1;
        }

        // Separator
        if y < inner.y + inner.height {
            let sep = "─".repeat(w);
            frame.render_widget(
                Paragraph::new(Span::styled(sep, Style::default().fg(theme::FG_DIM)))
                    .style(Style::default().bg(theme::BG)),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y += 1;
        }

        // Body area
        let body_height = (inner.y + inner.height).saturating_sub(y) as usize;
        if body_height > 0 && self.compose_field == ComposeField::Body {
            self.compose_ensure_cursor_visible(body_height);
        }

        let is_body_active = self.compose_field == ComposeField::Body;

        // Render body lines
        for vi in 0..body_height {
            let line_idx = self.compose_body_scroll + vi;
            let row_y = y + vi as u16;
            if row_y >= inner.y + inner.height {
                break;
            }

            let row_area = Rect::new(inner.x, row_y, inner.width, 1);

            if line_idx < self.compose_body.len() {
                let line_text = &self.compose_body[line_idx];
                let is_quote = line_text.starts_with("> ");

                let style = if is_quote {
                    Style::default().fg(theme::COMPOSE_QUOTE).bg(theme::BG)
                } else {
                    Style::default().fg(theme::FG_TEXT).bg(theme::BG)
                };

                // If this is the cursor line in body mode, render with cursor
                if is_body_active && line_idx == self.compose_cursor_row {
                    let chars: Vec<char> = line_text.chars().collect();
                    let mut spans = Vec::new();
                    for (ci, &ch) in chars.iter().enumerate() {
                        if ci == self.compose_cursor_col {
                            spans.push(Span::styled(
                                ch.to_string(),
                                Style::default().fg(theme::BG).bg(theme::COMPOSE_CURSOR),
                            ));
                        } else {
                            spans.push(Span::styled(ch.to_string(), style));
                        }
                    }
                    // Cursor at end of line
                    if self.compose_cursor_col >= chars.len() {
                        spans.push(Span::styled(
                            " ",
                            Style::default().fg(theme::BG).bg(theme::COMPOSE_CURSOR),
                        ));
                    }
                    frame.render_widget(Paragraph::new(Line::from(spans)), row_area);
                } else {
                    frame.render_widget(Paragraph::new(line_text.as_str()).style(style), row_area);
                }
            } else {
                // Empty line below content
                frame.render_widget(
                    Paragraph::new("").style(Style::default().bg(theme::BG)),
                    row_area,
                );
            }
        }

        // Render autocomplete dropdown as overlay
        if self.compose_show_suggestions && !self.compose_suggestions.is_empty() {
            let dropdown_y = match self.compose_field {
                ComposeField::To => inner.y + 2,
                ComposeField::Cc => inner.y + 3,
                ComposeField::Bcc => inner.y + 4,
                _ => inner.y + 2,
            };
            let dropdown_h = self.compose_suggestions.len().min(10) as u16;
            let dropdown_w = inner.width.min(50);
            let dropdown_x = inner.x + label_w as u16;

            if dropdown_y + dropdown_h <= area.y + area.height {
                let dropdown_area = Rect::new(dropdown_x, dropdown_y, dropdown_w, dropdown_h);
                frame.render_widget(Clear, dropdown_area);

                for (i, suggestion) in self.compose_suggestions.iter().enumerate() {
                    let sy = dropdown_y + i as u16;
                    if sy >= dropdown_y + dropdown_h {
                        break;
                    }
                    let s_area = Rect::new(dropdown_x, sy, dropdown_w, 1);
                    let is_sel = i == self.compose_suggestion_selected;
                    let style = if is_sel {
                        Style::default().fg(theme::FG_TEXT).bg(theme::BG_SELECTED)
                    } else {
                        Style::default()
                            .fg(theme::FG_TEXT)
                            .bg(theme::COMPOSE_DROPDOWN_BG)
                    };
                    let text = truncate_str(suggestion, dropdown_w as usize);
                    frame.render_widget(Paragraph::new(text).style(style), s_area);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_compose_field(
        &self,
        frame: &mut Frame,
        x: u16,
        y: u16,
        width: u16,
        label: &str,
        value: &str,
        is_active: bool,
    ) {
        let area = Rect::new(x, y, width, 1);
        let label_style = if is_active {
            Style::default()
                .fg(theme::COMPOSE_FIELD_ACTIVE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::FG_DIM)
        };

        if is_active {
            // Render with cursor at end
            let mut spans = vec![Span::styled(label, label_style)];
            let chars: Vec<char> = value.chars().collect();
            let visible_w = (width as usize).saturating_sub(label.len() + 1);
            let start = if chars.len() > visible_w {
                chars.len() - visible_w
            } else {
                0
            };
            for &ch in &chars[start..] {
                spans.push(Span::styled(
                    ch.to_string(),
                    Style::default().fg(theme::FG_TEXT).bg(theme::BG),
                ));
            }
            spans.push(Span::styled(
                " ",
                Style::default().fg(theme::BG).bg(theme::COMPOSE_CURSOR),
            ));
            frame.render_widget(
                Paragraph::new(Line::from(spans)).style(Style::default().bg(theme::BG)),
                area,
            );
        } else {
            let line = Line::from(vec![
                Span::styled(label, label_style),
                Span::styled(value, Style::default().fg(theme::FG_TEXT)),
            ]);
            frame.render_widget(
                Paragraph::new(line).style(Style::default().bg(theme::BG)),
                area,
            );
        }
    }

    fn render_filebrowser(&mut self, frame: &mut Frame, area: Rect) {
        let title = format!(" {} ", self.filebrowser_path.display());
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(theme::COMPOSE_BORDER))
            .title(title)
            .title_style(Style::default().fg(theme::SENDER_COLOR))
            .style(Style::default().bg(theme::BG));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let visible_height = inner.height as usize;

        if self.filebrowser_selected < self.filebrowser_scroll {
            self.filebrowser_scroll = self.filebrowser_selected;
        } else if self.filebrowser_selected >= self.filebrowser_scroll + visible_height {
            self.filebrowser_scroll = self.filebrowser_selected - visible_height + 1;
        }

        if self.filebrowser_entries.is_empty() {
            frame.render_widget(
                Paragraph::new("(empty directory)")
                    .style(Style::default().fg(theme::FG_DIM).bg(theme::BG)),
                inner,
            );
            return;
        }

        for (vi, idx) in (self.filebrowser_scroll..self.filebrowser_entries.len())
            .enumerate()
            .take(visible_height)
        {
            let y = inner.y + vi as u16;
            let row_area = Rect::new(inner.x, y, inner.width, 1);
            let entry = &self.filebrowser_entries[idx];
            let is_selected = idx == self.filebrowser_selected;
            let bg = if is_selected {
                theme::BG_SELECTED
            } else {
                theme::BG
            };

            let icon = if entry.is_dir { "/ " } else { "  " };
            let size_str = if entry.is_dir {
                String::new()
            } else {
                crate::smtp::format_attachment_size(entry.size)
            };

            let name_w = (inner.width as usize).saturating_sub(icon.len() + size_str.len() + 2);
            let display_name = truncate_str(&entry.name, name_w);
            let padding = name_w.saturating_sub(display_name.chars().count());

            let spans = vec![
                Span::styled(icon, Style::default().fg(theme::SENDER_COLOR).bg(bg)),
                Span::styled(
                    display_name,
                    if entry.is_dir {
                        Style::default().fg(theme::SENDER_COLOR).bg(bg)
                    } else {
                        Style::default().fg(theme::FG_TEXT).bg(bg)
                    },
                ),
                Span::styled(" ".repeat(padding), Style::default().bg(bg)),
                Span::styled(
                    format!("  {}", size_str),
                    Style::default().fg(theme::FG_DIM).bg(bg),
                ),
            ];
            frame.render_widget(Paragraph::new(Line::from(spans)), row_area);
        }

        if self.filebrowser_entries.len() > visible_height {
            let mut scrollbar_state = ScrollbarState::new(self.filebrowser_entries.len())
                .position(self.filebrowser_selected);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .style(Style::default().fg(theme::FG_DIM)),
                inner,
                &mut scrollbar_state,
            );
        }
    }

    fn help_bindings(&self) -> Vec<(&str, &str)> {
        match self.view {
            ViewMode::FolderSelect => vec![
                ("↑/↓", "Navigate folders"),
                ("Tab", "Expand/collapse account"),
                ("→/Enter", "Select folder"),
                ("←/Esc", "Back to list"),
                ("q", "Quit"),
                ("?", "Toggle this help"),
            ],
            ViewMode::List => {
                let mut keys = vec![
                    ("↑/↓", "Navigate emails"),
                    ("→/Enter", "Open email"),
                    ("←", "Open folder selector"),
                    ("PgUp/PgDn", "Page up/down"),
                    ("Home/g", "Go to top"),
                    ("End/G", "Go to bottom"),
                ];
                if self.thread_mode {
                    keys.extend_from_slice(&[
                        ("Tab/l", "Expand/collapse thread"),
                        ("Space", "Jump to next thread"),
                        ("Shift+Tab", "Jump to prev thread"),
                        ("t", "Switch to flat view"),
                    ]);
                } else {
                    keys.push(("t", "Switch to threaded view"));
                }
                keys.extend_from_slice(&[
                    ("n", "Compose new email"),
                    ("/", "Search"),
                    ("F", "Folder selector"),
                    ("q", "Quit"),
                    ("?", "Toggle this help"),
                ]);
                keys
            }
            ViewMode::Detail => {
                let mut keys = vec![];
                match self.detail_mode {
                    DetailMode::Text => {
                        keys.extend_from_slice(&[
                            ("←/Esc", "Back to list"),
                            ("↑/↓", "Scroll"),
                            ("Space/PgDn", "Page down"),
                            ("PgUp", "Page up"),
                            ("Home/End", "Top/bottom"),
                            ("Shift+←", "Previous email"),
                            ("Shift+→", "Next email"),
                            ("Shift+PgUp", "Prev in thread"),
                            ("Shift+PgDn", "Next in thread"),
                        ]);
                    }
                    DetailMode::Attachments => {
                        keys.extend_from_slice(&[
                            ("Esc", "Back to text mode"),
                            ("↑/↓/←/→", "Select attachment"),
                            ("Enter", "Open with xdg-open"),
                            ("s", "Save to ~/Downloads"),
                        ]);
                    }
                    DetailMode::Links => {
                        keys.extend_from_slice(&[
                            ("Esc", "Back to text mode"),
                            ("↑/↓/←/→", "Select link"),
                            ("Enter", "Open in browser"),
                        ]);
                    }
                }
                keys.extend_from_slice(&[
                    ("/", "Cycle mode (text/att/links)"),
                    ("n", "Compose new email"),
                    ("r", "Reply"),
                    ("f", "Forward"),
                    ("h", "Toggle raw headers"),
                    ("v", "Open HTML in browser"),
                    ("q", "Quit"),
                    ("?", "Toggle this help"),
                ]);
                keys
            }
            ViewMode::Search => vec![
                ("←/Esc", "Back to list"),
                ("→/Enter", "Search / open result"),
                ("↑/↓", "Navigate results"),
                ("Backspace", "Delete character"),
                ("?", "Toggle this help"),
            ],
            ViewMode::Compose => {
                let mut keys = vec![
                    ("Ctrl+Enter", "Send message"),
                    ("Ctrl+S", "Save draft"),
                    ("Esc", "Cancel compose"),
                    ("Tab", "Next field"),
                    ("Shift+Tab", "Previous field"),
                    ("Ctrl+A", "Attach file"),
                    ("Ctrl+D", "Remove last attachment"),
                ];
                if self.compose_senders.len() > 1 {
                    keys.push(("←/→ on From", "Cycle sender identity"));
                }
                if self.compose_field == ComposeField::FileBrowser {
                    keys.extend_from_slice(&[
                        ("↑/↓", "Navigate files"),
                        ("Enter", "Select file / enter dir"),
                        ("Backspace", "Parent directory"),
                        (".", "Toggle hidden files"),
                    ]);
                }
                keys
            }
        }
    }

    fn render_help_overlay(&self, frame: &mut Frame, area: Rect) {
        let bindings = self.help_bindings();
        let max_key_w = bindings.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
        let max_desc_w = bindings.iter().map(|(_, d)| d.len()).max().unwrap_or(0);
        let popup_w = (max_key_w + max_desc_w + 7).min(area.width as usize);
        let popup_h = (bindings.len() + 2).min(area.height as usize);
        let x = area.x + (area.width.saturating_sub(popup_w as u16)) / 2;
        let y = area.y + (area.height.saturating_sub(popup_h as u16)) / 2;
        let popup_area = Rect::new(x, y, popup_w as u16, popup_h as u16);

        frame.render_widget(Clear, popup_area);

        let title = match self.view {
            ViewMode::FolderSelect => " Help: Folders ",
            ViewMode::List => " Help: List ",
            ViewMode::Detail => " Help: Detail ",
            ViewMode::Search => " Help: Search ",
            ViewMode::Compose => " Help: Compose ",
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::HELP_BORDER))
            .title(title)
            .title_style(
                Style::default()
                    .fg(theme::HELP_TITLE)
                    .add_modifier(Modifier::BOLD),
            )
            .style(Style::default().bg(theme::HELP_BG));
        let inner = block.inner(popup_area);
        frame.render_widget(block, popup_area);

        let visible_height = inner.height as usize;
        let scroll = self.help_scroll.min(bindings.len().saturating_sub(visible_height));

        for (i, (key, desc)) in bindings.iter().enumerate().skip(scroll).take(visible_height) {
            let row_y = inner.y + (i - scroll) as u16;
            if row_y >= inner.y + inner.height {
                break;
            }
            let padded_key = format!("{:>width$}", key, width = max_key_w);
            let line = Line::from(vec![
                Span::styled(
                    format!(" {} ", padded_key),
                    Style::default()
                        .fg(theme::HELP_KEY)
                        .bg(theme::HELP_BG)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" {}", desc),
                    Style::default().fg(theme::HELP_DESC).bg(theme::HELP_BG),
                ),
            ]);
            frame.render_widget(
                Paragraph::new(line),
                Rect::new(inner.x, row_y, inner.width, 1),
            );
        }
    }

    fn spinner_prefix(&self) -> String {
        if self.spinner_active {
            let c = SPINNER_FRAMES[self.spinner_tick % SPINNER_FRAMES.len()];
            format!("{} ", c)
        } else {
            String::new()
        }
    }

    fn render_status_bar(&self, frame: &mut Frame, area: Rect) {
        let (keys, info) = match self.view {
            ViewMode::FolderSelect => (
                vec![("?", "help")],
                format!(" {}/{}", self.current_account, self.current_folder),
            ),
            ViewMode::List => {
                let keys = vec![("?", "help")];

                let status_display = if !self.status_msg.is_empty() {
                    format!("{}{}", self.spinner_prefix(), self.status_msg)
                } else if self.spinner_active {
                    format!("{}syncing", self.spinner_prefix())
                } else {
                    "jamail".to_string()
                };
                let folder_label = if self.is_global_inbox {
                    "All Inboxes".to_string()
                } else if let Some(ref vf) = self.virtual_folder {
                    vf.clone()
                } else {
                    format!("{}/{}", self.current_account, self.current_folder)
                };
                let info = if self.thread_mode {
                    let thread_count = self.threaded_view.threads.len();
                    format!(
                        " {}  {} threads, {} emails | {}",
                        folder_label,
                        thread_count,
                        self.emails.len(),
                        status_display,
                    )
                } else {
                    format!(
                        " {}  {} emails | {}",
                        folder_label,
                        self.emails.len(),
                        status_display,
                    )
                };
                (keys, info)
            }
            ViewMode::Detail => {
                let keys = vec![("?", "help")];
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
                vec![("?", "help")],
                format!(" {} results", self.search_results.len()),
            ),
            ViewMode::Compose => {
                let keys = match self.compose_field {
                    ComposeField::FileBrowser => vec![
                        ("↑↓", "navigate"),
                        ("Enter", "select"),
                        ("Bksp", "parent"),
                        (".", "hidden"),
                        ("Esc", "cancel"),
                    ],
                    ComposeField::Body => vec![
                        ("C-Enter", "send"),
                        ("C-s", "draft"),
                        ("C-a", "attach"),
                        ("C-d", "rm attach"),
                        ("S-Tab", "prev field"),
                        ("Esc", "cancel"),
                    ],
                    ComposeField::From => vec![
                        ("←→", "change sender"),
                        ("Tab", "next"),
                        ("Esc", "cancel"),
                    ],
                    _ => vec![
                        ("C-Enter", "send"),
                        ("Tab", "next"),
                        ("S-Tab", "prev"),
                        ("C-a", "attach"),
                        ("Esc", "cancel"),
                    ],
                };

                let mode_label = match self.compose_mode {
                    ComposeMode::New => "New",
                    ComposeMode::Reply => "Reply",
                    ComposeMode::Forward => "Forward",
                };
                let att_count = self.compose_attachments.len();
                let info = if att_count > 0 {
                    format!(" {}  {} attachment(s)", mode_label, att_count)
                } else {
                    format!(" {}", mode_label)
                };
                (keys, info)
            }
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

fn generate_text_preview(data: &[u8], mime_type: &str, filename: &str) -> Option<String> {
    use std::process::Command;

    // Direct text display for text/* types
    if mime_type.starts_with("text/") {
        let text = String::from_utf8_lossy(data);
        return Some(text.chars().take(5000).collect());
    }

    // For other types, write to temp file and try external tools
    let tmp_path = format!("/tmp/jamail_preview_{}", std::process::id());
    let tmp_file = format!("{}/{}", tmp_path, filename);
    std::fs::create_dir_all(&tmp_path).ok()?;
    std::fs::write(&tmp_file, data).ok()?;

    let output = match mime_type {
        "application/pdf" => Command::new("pdftotext")
            .args(["-l", "3", &tmp_file, "-"])
            .output()
            .ok(),
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        | "application/msword" => Command::new("pandoc")
            .args(["--to", "plain", &tmp_file])
            .output()
            .ok()
            .or_else(|| Command::new("docx2txt").arg(&tmp_file).output().ok()),
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        | "application/vnd.ms-excel"
        | "text/csv" => Command::new("pandoc")
            .args(["--to", "plain", &tmp_file])
            .output()
            .ok(),
        _ => None,
    };

    let _ = std::fs::remove_dir_all(&tmp_path);

    output.and_then(|o| {
        if o.status.success() {
            let text = String::from_utf8_lossy(&o.stdout).into_owned();
            if text.trim().is_empty() {
                None
            } else {
                Some(text.chars().take(5000).collect())
            }
        } else {
            None
        }
    })
}

/// Get the display name for a folder (last path component).
fn parse_hex_color(hex: &str) -> Option<Color> {
    let hex = hex.strip_prefix('#').unwrap_or(hex);
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

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
