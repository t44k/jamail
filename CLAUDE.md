# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

jamail is a terminal email client built with ratatui/crossterm. It connects to IMAP servers, caches all mail locally in a compressed SQLite database, and provides instant full-text search via FTS5.

Config is read from `~/.config/jamail/config.yaml`. Database lives at `~/.local/share/jamail/mail.db`.

Rust edition 2024 — uses let-chain syntax (`if let ... && let ...`).

## Build Commands

```bash
cargo build              # compile
cargo run                # run (needs IMAP config and a TTY)
cargo clippy             # lint
cargo fmt                # format
```

Unit tests live inline in each module under `#[cfg(test)]` (`cargo test`). They cover pure logic (config parsing, folder ordering, notification triggers, draft raw-message building, status-label text) and DB/App state transitions via `MailDb::open_in_memory()` and a `test_app()` helper — not TUI rendering or live IMAP/SMTP.

## Architecture

### Threading Model

Two threads communicate via `mpsc::channel<SyncEvent>`:

- **Main thread** (main.rs): TUI event loop. Polls keyboard/mouse events with 200ms timeout, drains sync events via non-blocking `try_recv()`, re-renders every iteration. Reads from the database for email content.
- **Background sync thread** (sync.rs): Owns the IMAP connection. Syncs all folders sequentially, then IDLEs on the currently viewed folder. Auto-reconnects on error (5s backoff). Writes to the database and sends progress events to the main thread.

Both threads open their own `MailDb` connection. SQLite WAL mode allows concurrent reads and writes.

### Multi-Account & Multi-Folder

- Multiple IMAP accounts are configured in `config.yaml` (IndexMap preserves order)
- Each account can optionally filter/order folders via a `folders` list in config
- `SyncControl` (Arc) allows the main thread to communicate folder changes to the sync thread:
  - `current_folder: RwLock<String>` — which folder to IDLE on (no thread restart needed)
  - `shutdown: AtomicBool` — signal to stop (set when switching accounts)
  - `folder_filter: RwLock<Vec<String>>` — which folders to sync
- Account switching shuts down the old sync thread and spawns a new one
- Folder switching within the same account just updates `SyncControl` + loads cached emails

### Module Responsibilities

- **main.rs** — Terminal setup/teardown, event loop dispatch, account/folder switching, compose key dispatch, SMTP send handler, mouse text extraction, clipboard copy
- **app.rs** — All UI state and rendering (ViewMode::FolderSelect/List/Detail/Search/Compose), folder tree, vertical folder label, mouse selection tracking, compose view (body editor, autocomplete, file browser)
- **mail.rs** — IMAP protocol: connect, list_folders, sync_folder (incremental via UIDVALIDITY), MIME parsing, IMAP IDLE, date parsing, HTML→text conversion (via w3m subprocess)
- **db.rs** — SQLite schema (v4: account+folder scoped; `local_messages` tracks draft/sending/send_error/sent status plus independent upload_status/upload_error for remote Sent/Draft folder uploads), zstd compression/decompression, FTS5 search, schema migration, CRUD operations, known-address extraction for autocomplete
- **sync.rs** — Background thread lifecycle: SyncControl (current_folder, folder_filter, mark_seen_queue, upload_queue), multi-folder sync loop, SyncEvent enum (includes UploadComplete/UploadError for Sent/Draft folder APPENDs)
- **config.rs** — YAML config deserialization (JamailConfig, JamailAccount, ImapConfig, AuthConfig, SmtpConfig). `JamailAccount` also carries `senders` (multiple From identities), `sent_folder`/`draft_folder` (remote upload destinations, each independently `Option<String>` — unset disables upload), `notify_folders` (desktop-notification trigger folders), and `show_unlisted_folders` (bool, default `false` — when `folders` is set, also sync/show remote folders not listed there) — all optional and backward-compatible
- **smtp.rs** — SMTP email sending via `lettre` (blocking transport, STARTTLS/implicit TLS, plain text and multipart with attachments); `send_email` returns the raw sent bytes for Sent-folder upload; `build_draft_raw` builds a tolerant (empty-recipients-OK) raw message for Draft-folder upload
- **notify.rs** — Best-effort desktop notification (`notify-send`) + sound (`canberra-gtk-play`/`paplay`/`pw-play`, tried in order) for accounts with `notify_folders` configured; no-ops silently when the tools aren't installed, same pattern as clipboard copy
- **theme.rs** — Color palette and style constants (dark theme, all RGB values, compose view colors, Sent/Draft status badge colors)
- **thread.rs** — Email threading via Message-ID/In-Reply-To/References + subject-based fallback; caches `newest_status_label` from the newest email for thread-summary row rendering

### Key Design Decisions

- **Email bodies are zstd-compressed** (level 3) in the database. Only decompressed when opening detail view. Preview text (first 200 chars) is stored uncompressed for list rendering.
- **Emails have an autoincrement `id`** (PK) separate from IMAP `uid`. UID is only unique within (account, folder). All UI lookups use `id`.
- **FTS5 is a standalone table** (no `content=` directive) — stores its own inverted index. FTS rowid = emails.id.
- **Database schema versioning** — `schema_version` table tracks version. On upgrade, all tables are dropped and recreated (DB is just a cache).
- **IMAP sync is incremental** — tracks UIDVALIDITY and last_uid per (account, folder) in sync_state table.
- **`BODY.PEEK[]`** is used instead of `BODY[]` to avoid marking messages as \Seen on the server.
- **Auth supports `password` and `command` types** — command runs a shell command and captures stdout (e.g., `pass show email/work`). Reused for both IMAP and SMTP.
- **SMTP is optional** — `SmtpConfig` is an `Option` on `JamailAccount`. Compose/reply/forward UI is always available; sending shows an error if SMTP is not configured. Uses `lettre` crate with blocking transport (no async runtime).
- **Compose state lives on `App`** — all compose fields (to, cc, bcc, subject, body lines, cursor position, attachments, autocomplete, file browser) are flat fields on the `App` struct. `ComposeField` enum tracks which field is active; `ComposeMode` tracks New/Reply/Forward.
- **Body editor operates on `Vec<String>`** — one element per line, with `(cursor_row, cursor_col)` in char units. `char_to_byte_pos()` converts char index to byte offset for correct string manipulation.
- **Contact autocomplete** — `db.get_known_addresses()` extracts distinct from/to addresses from the cache. Filtered by substring match on the current token (text after last comma) in address fields. Up to 10 suggestions shown as an overlay.
- **Forward auto-attaches originals** — attachment data is extracted from DB, written to `/tmp/jamail_fwd_<id>/`, and added to `compose_attachments`. Temp files are cleaned up on send or cancel.
- **Date parsing** uses `mailparse::dateparse` as primary parser, with a custom `normalize_timezone()` preprocessing step that maps non-RFC2822 timezone abbreviations (CEST, BST, JST, etc.) to numeric offsets before parsing.
- **Mouse selection** is constrained to the detail panel's inner rect (`selectable_area`). Text is extracted from ratatui's `CompletedFrame` buffer on mouse release and copied via wl-copy/xclip/xsel.
- **Threads store indices, not clones** — `Thread.email_indices: Vec<usize>` references into `App.emails`. Summary data (`newest_from`, `has_attachments`) is cached on the Thread struct to avoid lookups during rendering. `build_threads()` takes `&[Email]`.
- **Parallel email processing** — `sync_folder` uses `std::thread::scope` to run `process_raw_email` calls concurrently (w3m subprocesses). Capped at `PROCESS_PARALLELISM` (8) concurrent threads per sub-batch to limit peak memory from attachment data.
- **SQLite memory limits** — `PRAGMA cache_size = -4000` (4MB cap per connection), `PRAGMA mmap_size = 0` (no memory-mapped I/O).
- **Multiple sender identities** — `accounts.<name>.senders` (optional) lists "From" identities cycled via Left/Right on the compose From field. `App.compose_senders` is always rebuilt from config at compose-entry time, so the From field can only ever hold a configured value; `compose_from_is_valid()` re-checks this immediately before a send is enqueued.
- **Folder display order matches sync order** — `mail::order_folders()` is the single source of truth for both what the sync thread syncs (`SyncControl.folder_filter`) and what the folder selector shows (`App.rebuild_folder_tree`), keyed off the same `accounts.<name>.folders` config. Unconfigured: INBOX first, then alphabetical (deterministic default, since IMAP LIST order and DB-cache order aren't guaranteed to agree or stay stable). When `folders` is set, `accounts.<name>.show_unlisted_folders` (default `false`) controls whether remote folders absent from that list are appended after it (same INBOX-first/alphabetical order) instead of being hidden/unsynced entirely.
- **Folder-selection cursor survives tree rebuilds** — `App.rebuild_folder_tree()` captures the currently-selected row's identity (account+folder name, or the virtual/global-inbox/account sentinel) before rebuilding, then relocates the cursor to that same row afterward regardless of index shifts from reordering, expand/collapse, or newly-synced folders; falls back to the first row when that identity is no longer present (row removed, account collapsed). `App.enter_folder_select()` relies on this instead of resetting the cursor, so reopening the folder selector returns to where you left it.
- **Remote Sent/Draft folder upload is opt-in and one-shot** — `sent_folder`/`draft_folder` are independent `Option<String>` keys; unset means "local cache only" (no APPEND attempted). A sent message is uploaded once (after a successful SMTP send); a draft is uploaded once, on its *first* explicit `Ctrl+S` save only — later edits update the local cache but do not re-upload, so there's no remote delete/replace-on-update logic to track a remote UID. Uploads go through `SyncControl.upload_queue`, drained by the sync thread (which already owns a live IMAP connection) alongside `mark_seen_queue`; results come back as `SyncEvent::UploadComplete`/`UploadError`.
- **Sent/Draft in-flight state never disappears silently** — `local_messages.status` includes `sending` and `send_error` (not just `draft`/`sent`), and `get_drafts()` includes all three non-`sent` states so a message stays visible (with a "Sending…"/"Send failed: ..." badge) through the whole send lifecycle. `upload_status`/`upload_error` track the independent Sent/Draft-folder upload outcome the same way. `Email.status_label` (set only for local Drafts/Sent rows) carries the badge text into both the flat and threaded list renderers.
- **Desktop notifications** — `notify_folders` (optional, per account) lists folders that trigger a `notify-send` desktop notification + best-effort sound (`canberra-gtk-play`/`paplay`/`pw-play`) on new mail, mirroring the existing external-tool-with-graceful-fallback pattern (w3m, xdg-open, wl-copy/xclip/xsel). Unset means no notifications. Fires once per `FolderComplete` sync event (not per message), so an initial full backfill sync produces one notification with the total count rather than one per message.

### Adding Keyboard Shortcuts

1. Add the key handler in `main.rs` under the appropriate `ViewMode` match arm
2. Add the action method in `app.rs` on `App`
3. Update `render_status_bar()` in app.rs to show the new key hint

### External Tool Dependencies

- **w3m** — HTML-to-text conversion (optional; falls back to tag stripping)
- **xdg-open** — Opening HTML email in browser (`v` key)
- **wl-copy / xclip / xsel** — Clipboard copy for mouse selection (tried in order)
