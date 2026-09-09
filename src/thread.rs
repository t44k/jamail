use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Local};

use crate::mail::{Email, normalize_subject};

pub struct Thread {
    pub id: String,
    pub subject: String,
    pub newest_date: DateTime<Local>,
    pub message_count: usize,
    pub unread_count: usize,
    /// Indices into the shared `App.emails` vec (newest-first order).
    pub email_indices: Vec<usize>,
    /// Cached from the newest email for rendering thread summary rows.
    pub newest_from: String,
    /// Whether any email in this thread has attachments.
    pub has_attachments: bool,
    /// Cached from the newest email's `status_label` (Drafts/Sent in-flight
    /// state); `None` for real IMAP-synced threads.
    pub newest_status_label: Option<String>,
}

#[derive(Clone)]
pub enum DisplayRow {
    ThreadSummary { thread_idx: usize },
    ThreadEmail { thread_idx: usize, email_idx: usize },
}

pub struct ThreadedView {
    pub threads: Vec<Thread>,
    pub expanded: HashSet<String>,
    pub rows: Vec<DisplayRow>,
}

impl ThreadedView {
    pub fn new() -> Self {
        Self {
            threads: Vec::new(),
            expanded: HashSet::new(),
            rows: Vec::new(),
        }
    }
}

impl Default for ThreadedView {
    fn default() -> Self {
        Self::new()
    }
}

// Union-Find data structure for merging thread groups
struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            rank: vec![0; n],
        }
    }

    fn find(&mut self, x: usize) -> usize {
        if self.parent[x] != x {
            self.parent[x] = self.find(self.parent[x]);
        }
        self.parent[x]
    }

    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra == rb {
            return;
        }
        if self.rank[ra] < self.rank[rb] {
            self.parent[ra] = rb;
        } else if self.rank[ra] > self.rank[rb] {
            self.parent[rb] = ra;
        } else {
            self.parent[rb] = ra;
            self.rank[ra] += 1;
        }
    }
}

/// Build threads from a flat list of emails using Message-ID/In-Reply-To/References headers.
/// Falls back to subject-based grouping for emails without threading headers.
///
/// Stores indices into `emails` rather than cloning the emails themselves.
pub fn build_threads(emails: &[Email]) -> Vec<Thread> {
    if emails.is_empty() {
        return Vec::new();
    }

    let n = emails.len();
    let mut uf = UnionFind::new(n);

    // Map message_id -> email index (for non-empty message_ids)
    let mut msgid_to_idx: HashMap<&str, usize> = HashMap::new();
    for (i, email) in emails.iter().enumerate() {
        if !email.message_id.is_empty() {
            msgid_to_idx.insert(&email.message_id, i);
        }
    }

    // Phase 1: Union by References and In-Reply-To
    for (i, email) in emails.iter().enumerate() {
        // Union with In-Reply-To
        if !email.in_reply_to.is_empty()
            && let Some(&j) = msgid_to_idx.get(email.in_reply_to.as_str())
        {
            uf.union(i, j);
        }

        // Union with all message-ids in References
        if !email.references.is_empty() {
            let ref_ids: Vec<&str> = email.references.split_whitespace().collect();
            // Union this email with any referenced email we have
            for ref_id in &ref_ids {
                if let Some(&j) = msgid_to_idx.get(*ref_id) {
                    uf.union(i, j);
                }
            }
            // Also union consecutive references with each other
            for pair in ref_ids.windows(2) {
                if let (Some(&a), Some(&b)) = (msgid_to_idx.get(pair[0]), msgid_to_idx.get(pair[1]))
                {
                    uf.union(a, b);
                }
            }
        }
    }

    // Phase 2: Subject-based fallback for emails with no threading headers
    // Build a map of normalized subject -> (thread root, newest date)
    // NOTE: We intentionally do NOT union threads by subject here. Different threads
    // can share a subject (e.g., "Mail Delivery Notification" bounces referencing
    // unrelated conversations). Unioning by subject would transitively merge those
    // unrelated conversations into a single mega-thread.
    let mut subject_to_root: HashMap<String, (usize, DateTime<Local>)> = HashMap::new();

    // First pass: register subjects from emails that have threading headers
    for (i, email) in emails.iter().enumerate() {
        let has_headers = !email.in_reply_to.is_empty() || !email.references.is_empty();
        if has_headers {
            let norm = normalize_subject(&email.subject);
            if norm.chars().count() >= 10 {
                let root = uf.find(i);
                subject_to_root.entry(norm).or_insert((root, email.date));
            }
        }
    }

    // Second pass: merge orphan emails (no threading headers) by subject
    for (i, email) in emails.iter().enumerate() {
        let has_headers = !email.in_reply_to.is_empty() || !email.references.is_empty();
        if has_headers {
            continue;
        }
        let norm = normalize_subject(&email.subject);
        if norm.chars().count() < 10 {
            continue;
        }
        if let Some((root, newest)) = subject_to_root.get(&norm) {
            // Only merge if within 90 days of the newest message in the thread
            let days_apart = (email.date - *newest).num_days().unsigned_abs();
            if days_apart <= 90 {
                uf.union(i, *root);
            }
        }
    }

    // Collect groups
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let root = uf.find(i);
        groups.entry(root).or_default().push(i);
    }

    // Build Thread structs (index-based, no email cloning)
    let mut threads: Vec<Thread> = Vec::new();

    for (_root, mut indices) in groups {
        // Sort by date descending (newest first)
        indices.sort_by(|&a, &b| emails[b].date.cmp(&emails[a].date));

        if indices.is_empty() {
            continue;
        }

        let newest = &emails[indices[0]];
        let oldest = &emails[*indices.last().unwrap()];
        let newest_date = newest.date;
        let newest_from = newest.from.clone();
        let newest_status_label = newest.status_label.clone();
        let unread_count = indices.iter().filter(|&&i| emails[i].is_unread).count();
        let has_attachments = indices.iter().any(|&i| emails[i].has_attachments);
        let subject = normalize_subject(&oldest.subject);
        let id = if !oldest.message_id.is_empty() {
            oldest.message_id.clone()
        } else {
            format!("synth-{}", oldest.uid)
        };
        let message_count = indices.len();

        threads.push(Thread {
            id,
            subject,
            newest_date,
            message_count,
            unread_count,
            email_indices: indices,
            newest_from,
            has_attachments,
            newest_status_label,
        });
    }

    // Sort threads by newest_date descending
    threads.sort_by_key(|t| std::cmp::Reverse(t.newest_date));

    threads
}

/// Rebuild the flat display rows from threads + expansion state.
pub fn rebuild_rows(view: &mut ThreadedView) {
    view.rows.clear();
    for (thread_idx, thread) in view.threads.iter().enumerate() {
        view.rows.push(DisplayRow::ThreadSummary { thread_idx });
        if view.expanded.contains(&thread.id) {
            for email_idx in 0..thread.email_indices.len() {
                view.rows.push(DisplayRow::ThreadEmail {
                    thread_idx,
                    email_idx,
                });
            }
        }
    }
}
