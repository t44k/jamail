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

No tests exist yet. `cargo test` runs but there are zero test cases.

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

- **main.rs** — Terminal setup/teardown, event loop dispatch, account/folder switching, mouse text extraction, clipboard copy
- **app.rs** — All UI state and rendering (ViewMode::FolderSelect/List/Detail/Search), folder tree, vertical folder label, mouse selection tracking
- **mail.rs** — IMAP protocol: connect, list_folders, sync_folder (incremental via UIDVALIDITY), MIME parsing, IMAP IDLE, date parsing, HTML→text conversion (via w3m subprocess)
- **db.rs** — SQLite schema (v2: account+folder scoped), zstd compression/decompression, FTS5 search, schema migration, CRUD operations
- **sync.rs** — Background thread lifecycle: SyncControl, multi-folder sync loop, SyncEvent enum
- **config.rs** — YAML config deserialization (JamailConfig, JamailAccount, ImapConfig, AuthConfig)
- **theme.rs** — Color palette and style constants (dark theme, all RGB values)
- **thread.rs** — Email threading via Message-ID/In-Reply-To/References + subject-based fallback

### Key Design Decisions

- **Email bodies are zstd-compressed** (level 3) in the database. Only decompressed when opening detail view. Preview text (first 200 chars) is stored uncompressed for list rendering.
- **Emails have an autoincrement `id`** (PK) separate from IMAP `uid`. UID is only unique within (account, folder). All UI lookups use `id`.
- **FTS5 is a standalone table** (no `content=` directive) — stores its own inverted index. FTS rowid = emails.id.
- **Database schema versioning** — `schema_version` table tracks version. On upgrade, all tables are dropped and recreated (DB is just a cache).
- **IMAP sync is incremental** — tracks UIDVALIDITY and last_uid per (account, folder) in sync_state table.
- **`BODY.PEEK[]`** is used instead of `BODY[]` to avoid marking messages as \Seen on the server.
- **Auth supports `password` and `command` types** — command runs a shell command and captures stdout (e.g., `pass show email/work`).
- **Date parsing** uses `mailparse::dateparse` as primary parser, with a custom `normalize_timezone()` preprocessing step that maps non-RFC2822 timezone abbreviations (CEST, BST, JST, etc.) to numeric offsets before parsing.
- **Mouse selection** is constrained to the detail panel's inner rect (`selectable_area`). Text is extracted from ratatui's `CompletedFrame` buffer on mouse release and copied via wl-copy/xclip/xsel.
- **Threads store indices, not clones** — `Thread.email_indices: Vec<usize>` references into `App.emails`. Summary data (`newest_from`, `has_attachments`) is cached on the Thread struct to avoid lookups during rendering. `build_threads()` takes `&[Email]`.
- **Parallel email processing** — `sync_folder` uses `std::thread::scope` to run `process_raw_email` calls concurrently (w3m subprocesses). Capped at `PROCESS_PARALLELISM` (8) concurrent threads per sub-batch to limit peak memory from attachment data.
- **SQLite memory limits** — `PRAGMA cache_size = -4000` (4MB cap per connection), `PRAGMA mmap_size = 0` (no memory-mapped I/O).

### Adding Keyboard Shortcuts

1. Add the key handler in `main.rs` under the appropriate `ViewMode` match arm
2. Add the action method in `app.rs` on `App`
3. Update `render_status_bar()` in app.rs to show the new key hint

### External Tool Dependencies

- **w3m** — HTML-to-text conversion (optional; falls back to tag stripping)
- **xdg-open** — Opening HTML email in browser (`v` key)
- **wl-copy / xclip / xsel** — Clipboard copy for mouse selection (tried in order)
