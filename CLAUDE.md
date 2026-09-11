# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

jamail is a terminal email client built with ratatui/crossterm. It connects to IMAP servers, caches all mail locally in a compressed SQLite database, and provides instant full-text search via FTS5.

It also has optional CalDAV calendar sync, with its own terminal UI. It ships as **three binaries** sharing one library crate (`src/lib.rs`):

- **`jamaild`** (`src/bin/jamaild.rs` + `src/daemon.rs`) — a background daemon. Owns every configured account's IMAP connection/sync loop (plus, for any account with `caldav` configured, a CalDAV calendar sync loop — see `src/calsync.rs`) and all writes to the local cache, and is the sole place that decides to fire a desktop mail notification. Optionally also runs a real inbound CalDAV HTTP server (`daemon.caldav_server_listen`, off by default) exposing those same accounts' calendars to other CalDAV clients — see `src/caldav_server.rs`. Suitable for `systemd --user` (`contrib/systemd/jamaild.service`); syncs every account continuously from startup regardless of whether any client is connected.
- **`jamail`** (`src/bin/jamail.rs`) — the terminal UI for mail. Reads the cache directly and talks to `jamaild` over the IPC protocol in `src/ipc.rs` for anything needing a live IMAP session (which folder to IDLE on, mark-seen, Sent/Draft uploads) and for the sync-event stream. Auto-spawns a `jamaild` if none is reachable.
- **`jacal`** (`src/bin/jacal.rs` + `src/calapp.rs`) — a separate terminal UI for calendars: day/3-day/week/month/year views rendered as colored table grids (recurring events expanded to every occurrence in range, all-day events shown across every day they span), create/edit/delete, manual sync trigger, alarm notifications. Reads calendar data from the same local cache and talks to `jamaild` over the same IPC protocol to trigger a sync and queue event create/update/delete requests, mirroring how `jamail` queues Sent/Draft uploads. Only shows accounts with `caldav` configured; exits immediately with a clear message if none are. Auto-spawns a `jamaild` if none is reachable, same as `jamail`. See `src/calendar.rs`, `src/caldav.rs`, `src/calsync.rs`, and `src/calnotify.rs` module docs for the iCalendar parser/serializer, CalDAV client, daemon-side sync loop, and alarm notification scheduling, respectively — and their doc comments for explicit, deliberate scope limitations (editing/deleting a recurring event always acts on its master `VEVENT`, IANA-timezone-only, alarms only fire while `jacal` is running, etc).

Config is read from `~/.config/jamail/config.yaml` by **all three** binaries independently (each reads what it needs; only `jamaild` needs IMAP/CalDAV credentials, only `jamail` needs SMTP credentials for compose/send, only `jacal` needs `caldav` configured on at least one account). Database lives at `~/.local/share/jamail/mail.db`, opened independently by all three (SQLite WAL mode allows concurrent reads/writes from multiple connections, same as before the daemon split).

Rust edition 2024 — uses let-chain syntax (`if let ... && let ...`).

## Build Commands

```bash
cargo build --bins       # compile jamail, jamaild, and jacal
cargo run --bin jamail   # run the mail TUI (needs IMAP config and a TTY; auto-spawns jamaild)
cargo run --bin jacal    # run the calendar TUI (needs a caldav-configured account and a TTY; auto-spawns jamaild)
cargo run --bin jamaild  # run the daemon in the foreground (needs IMAP config, no TTY needed)
cargo clippy --all-targets -- -D warnings   # lint (lib + all three bins + tests/)
cargo fmt                # format
```

Unit tests live inline in each module under `#[cfg(test)]` (`cargo test`). They cover pure logic (config parsing, folder ordering, notification triggers, draft raw-message building, status-label text, iCalendar parsing/serialization in `src/calendar.rs`, calendar view navigation/form validation in `src/calapp.rs`, alarm scheduling in `src/calnotify.rs`), IPC framing/serialization/version-handshake/reconnect behavior (`src/ipc.rs`), daemon request-handling/client-broadcast/notification-dispatch/socket-lifecycle behavior over real `UnixStream` pairs (`src/daemon.rs`), CalDAV discovery/sync/CRUD/conflict behavior over a real local mock HTTP server (`src/caldav.rs`, `src/httpc.rs`), inbound CalDAV server discovery/auth/CRUD/REPORT behavior over a real local `TcpListener`, including round-trips through this crate's own `caldav.rs` client (`src/caldav_server.rs`), and DB/App state transitions via `MailDb::open_in_memory()` and a `test_app()` helper — not TUI rendering. `tests/smoke.rs` is a real end-to-end integration test: it spawns the actual compiled `jamaild`/`jamail` binaries and talks to the daemon over a real Unix socket (see that file's module doc for exactly what it covers and the one deliberate scope limitation — no live IMAP/TLS server is faked, since doing so would require weakening certificate validation in production code; the same limitation applies to `jacal`/CalDAV, verified instead by manual smoke runs against a real `jamaild` plus the local mock-HTTP-server tests in `src/caldav.rs`).

## Architecture

### Daemon/Client IPC (jamaild <-> jamail)

Replaces the old single-process "background sync thread" model. Full protocol
definition — framing, versioning, error semantics, socket path resolution,
lifecycle/shutdown — is documented in `src/ipc.rs`'s module doc comment;
daemon-side process/connection model is documented in `src/daemon.rs`'s. Summary:

- **Transport**: a Unix domain stream socket, `$XDG_RUNTIME_DIR/jamail/jamaild.sock`
  by default (overridable via `JAMAIL_SOCKET` env var or `daemon.socket_path` in
  config), directory `0700` / socket `0600`.
- **Framing**: `[u32 LE length][JSON payload]`, capped at `ipc::MAX_FRAME_BYTES`.
- **Handshake**: client sends `ClientHello{protocol_version}`; daemon replies
  `ServerHello::Ok` or `ServerHello::VersionMismatch` (then closes) — see
  `ipc::PROTOCOL_VERSION`/`ipc::is_compatible`.
- **Requests** (`jamail` -> `jamaild`, one `Response` each): `Ping`,
  `SetCurrentFolder{account,folder}` (IDLE-target hint only — never starts/stops
  syncing an account), `MarkSeen`, `EnqueueUpload` (base64'd raw message; result
  arrives later as an `UploadComplete`/`UploadError` event, correlated by
  `local_id`), `ForceReconnect`, `Shutdown` (graceful: sets every account's
  `sync::SyncControl.shutdown`, unlinks the socket, exits — same path SIGTERM/SIGINT
  trigger).
- **Events** (`jamaild` -> `jamail`, unsolicited, tagged with `account`): the wire
  form of `sync::SyncEvent`, produced by `ipc::wire_event`.
- **Client reconnect**: `ipc::IpcClient` owns a persistent connection with
  automatic reconnect/backoff, surfaced to the UI as `ClientEvent::{Connected,
  Disconnected, Fatal}` — `Fatal` (version mismatch) stops retrying; anything else
  keeps retrying indefinitely, mirroring the old sync thread's own 5s-backoff
  reconnect style.
- **Notification ownership**: `daemon::dispatch_sync_event` runs inside `jamaild`'s
  per-account event-pump thread and is the *only* place that decides to call
  `notify::notify_new_mail` — `jamail` never references the `notify` module at all
  (enforced by a test in `tests/smoke.rs`).

### Threading Model

**Inside `jamaild`**: one `sync::spawn_sync_thread` (unchanged from before the
daemon split) plus one "event pump" thread per configured account (the pump reads
that account's `SyncEvent`s, calls `daemon::dispatch_sync_event` for notification
delivery, then broadcasts the tagged event to every connected client). Each accepted
client connection gets a reader thread (`daemon::handle_client` — handshake +
request loop) and a writer thread (drains a per-client channel fed by both direct
responses and broadcast events, so only one thread ever writes to a given socket).

**Inside `jamail`**: the TUI event loop (unchanged in shape from before the daemon
split) polls keyboard/mouse events with a 200ms timeout and drains
`ipc::IpcClient::event_rx` via non-blocking `try_recv()` instead of an in-process
`mpsc::Receiver<SyncEvent>`. Reads from the database directly for email content, same
as always.

### Multi-Account & Multi-Folder

- Multiple IMAP accounts are configured in `config.yaml` (IndexMap preserves order)
- Each account can optionally filter/order folders via a `folders` list in config
- `jamaild` builds one `SyncControl` (Arc) per configured account at startup and never tears any of them down while running — this is what lets every account keep syncing, and notifications keep firing, regardless of which one (if any) a connected `jamail` is currently looking at:
  - `current_folder: RwLock<String>` — which folder to IDLE on (updated by `ipc::Request::SetCurrentFolder`, no thread restart needed)
  - `shutdown: AtomicBool` — signal to stop (set by `daemon::Daemon::trigger_graceful_shutdown` on `ipc::Request::Shutdown`/SIGTERM/SIGINT)
  - `folder_filter: RwLock<Vec<String>>` — which folders to sync
- "Switching accounts" in `jamail` is client-side only now: no thread spawn/teardown, just load the new account+folder from the local cache and send `SetCurrentFolder` so `jamaild` knows which one to prioritize for IDLE
- Folder switching within the same account works the same way — loads cached emails, sends `SetCurrentFolder`

### Module Responsibilities

- **lib.rs** — Declares every shared module as `pub mod`; both binaries depend on this one library crate
- **bin/jamail.rs** — The terminal client's `main`/event loop: terminal setup/teardown, event loop dispatch, folder/account navigation (now just a local cache load + an `ipc::Request::SetCurrentFolder`, no thread management), compose key dispatch, SMTP send handler, mouse text extraction, clipboard copy, daemon auto-spawn on startup
- **bin/jamaild.rs** — The daemon's `main`: thin wrapper calling `daemon::run()`
- **daemon.rs** — Daemon process/connection model: spawns one `sync::SyncControl`+sync thread+event-pump thread per configured account at startup (never torn down while running), accepts IPC client connections (`handle_client`: handshake, request loop, per-client writer thread, broadcast registration), `dispatch_sync_event` (the notification-ownership decision), socket bind/stale-socket recovery (`bind_or_recover`), SIGTERM/SIGINT + `Request::Shutdown` graceful-shutdown handling
- **ipc.rs** — The `jamaild`<->`jamail` wire protocol: versioned framing (`write_frame`/`read_frame`), `Request`/`Response`/`Event`/`ClientHello`/`ServerHello` message types, socket path resolution (`resolve_socket_path`, env var > config > XDG default), the client-side `IpcClient` connection manager (auto-reconnect with backoff, `Connected`/`Disconnected`/`Fatal` lifecycle events)
- **app.rs** — All UI state and rendering (ViewMode::FolderSelect/List/Detail/Search/Compose), folder tree, vertical folder label, mouse selection tracking, compose view (body editor, autocomplete, file browser)
- **mail.rs** — IMAP protocol: connect, list_folders, sync_folder (incremental via UIDVALIDITY), MIME parsing, IMAP IDLE, date parsing, HTML→text conversion (via w3m subprocess). `FolderInfo` also derives `Serialize`/`Deserialize` — it's reused verbatim as the wire type in `ipc::Event::FoldersLoaded`
- **db.rs** — SQLite schema (v6: account+folder scoped mail tables unchanged since v4; `calendars`/`calendar_events` added in v5; `calendar_sync_log` added in v6, an append-only change log the inbound CalDAV server's RFC 6578 `sync-collection` support reads to answer "what changed since token X"; `local_messages` tracks draft/sending/send_error/sent status plus independent upload_status/upload_error for remote Sent/Draft folder uploads), zstd compression/decompression, FTS5 search, schema migration, CRUD operations, known-address extraction for autocomplete. `get_email_account_uid_and_folder()` returns the owning account alongside uid/folder — needed since `jamaild` now syncs every account concurrently, so routing a mark-seen request by account (not just assuming "the current one") matters, notably from the cross-account Global Inbox view. **Both the v4->v5 and v5->v6 upgrades are additive** (`migrate_v4_to_v5_add_calendar_tables`, `migrate_v5_to_v6_add_sync_log`): each only adds its new tables and never drops/recreates existing ones, unlike every schema bump before v5 — picking up calendar support must not cost an existing install its cached mail (or, later, its already-synced calendar data). `calendar_events.local_status` (`CAL_STATUS_SYNCED`/`PENDING_CREATE`/`PENDING_UPDATE`/`PENDING_DELETE`/`CONFLICT`) mirrors `local_messages`' upload-queue pattern for local calendar writes awaiting sync. `get_calendar_events_in_range` returns a recurring event's row regardless of whether its own master `dtstart_utc`/`dtend_utc` overlaps the query window (a later occurrence can still fall inside it) — expanding to individual occurrences is `calendar::expand_occurrences`/`calapp::expand_recurring_events`'s job, not this query's
- **sync.rs** — `jamaild`-internal per-account sync thread lifecycle: SyncControl (current_folder, folder_filter, mark_seen_queue, upload_queue), multi-folder sync loop, SyncEvent enum (includes UploadComplete/UploadError for Sent/Draft folder APPENDs). Unchanged in shape from before the daemon split — `daemon.rs` now owns one instance of this per account instead of `jamail`'s old main.rs owning a single one for whichever account was active. `UploadKind` also derives `Serialize`/`Deserialize` — reused verbatim as the wire type in `ipc::Request::EnqueueUpload`/`ipc::Event::UploadComplete`/`UploadError`
- **config.rs** — YAML config deserialization (JamailConfig, JamailAccount, ImapConfig, AuthConfig, SmtpConfig). `JamailAccount` also carries `senders` (multiple From identities), `sent_folder`/`draft_folder` (remote upload destinations, each independently `Option<String>` — unset disables upload), `notify_folders` (desktop-notification trigger folders), and `show_unlisted_folders` (bool, default `false` — when `folders` is set, also sync/show remote folders not listed there) — all optional and backward-compatible. `JamailConfig.daemon: Option<DaemonConfig>` (optional `socket_path` override) is read independently by both binaries
- **smtp.rs** — SMTP email sending via `lettre` (blocking transport, STARTTLS/implicit TLS, plain text and multipart with attachments); stays entirely client-side (`jamail`'s `SendQueue`) since sending is a synchronous foreground action tied to the compose UI. `send_email` returns the raw sent bytes for Sent-folder upload; `build_draft_raw` builds a tolerant (empty-recipients-OK) raw message for Draft-folder upload. `SendJob`/`SendResult` carry `account` (captured at enqueue time) so a Sent-folder upload always targets the account the message was actually sent from, even if the user switches accounts in the UI before the background send completes
- **notify.rs** — Best-effort desktop notification (`notify-send`) + sound (`canberra-gtk-play`/`paplay`/`pw-play`, tried in order) for accounts with `notify_folders` configured; no-ops silently when the tools aren't installed, same pattern as clipboard copy. Called exclusively from `daemon.rs` (`dispatch_sync_event`/`default_notifier`) — `jamail` (the client) never references this module
- **theme.rs** — Color palette and style constants (dark theme, all RGB values, compose view colors, Sent/Draft status badge colors)
- **thread.rs** — Email threading via Message-ID/In-Reply-To/References + subject-based fallback; caches `newest_status_label` from the newest email for thread-summary row rendering
- **calendar.rs** — iCalendar (RFC 5545) `VEVENT` domain model, parser, and serializer. Standards-conscious subset: parses UID/DTSTART/DTEND/SUMMARY/DESCRIPTION/LOCATION/STATUS/ORGANIZER/ATTENDEE/RRULE/EXDATE/VALARM, timezone-aware via `chrono-tz` (IANA names only — a non-IANA `TZID` is an explicit `CalendarError::UnsupportedTimezone`, not a silent misparse). `expand_occurrences` (backed by the `rrule` crate) computes every occurrence of a recurring event overlapping a window, honoring `EXDATE`, for display purposes only — editing/deleting a recurring event always acts on the single master `VEVENT`; `RDATE`/`EXRULE` are preserved for round-tripping but not expanded. Edits patch the original raw `VEVENT` text in place (`VEvent::to_ics`) so properties this module doesn't model (`CATEGORIES`, `X-` extensions, etc.) survive unchanged
- **httpc.rs** — Minimal synchronous HTTP/1.1 client (arbitrary methods, headers, redirects, chunked/content-length bodies) built directly on `native_tls`/`std::net` rather than a new HTTP client dependency, used only by `caldav.rs`
- **caldav.rs** — Synchronous CalDAV (RFC 4791) client: RFC 4791 discovery (`current-user-principal` -> `calendar-home-set`), RFC 6578 `sync-collection` change sync with `calendar-query` full-listing fallback, and `GET`/`PUT`/`DELETE` with `If-Match`/`If-None-Match` ETag conflict protection. WebDAV multistatus XML parsed via `quick-xml`. Tested against a local mock HTTP server (see module doc for the TLS-untested scope limitation, mirroring `tests/smoke.rs`'s IMAP one)
- **calsync.rs** — `jamaild`-internal per-account CalDAV sync thread (`CalSyncControl`, poll loop since CalDAV has no IDLE equivalent — interval from `config::CalDavConfig::poll_interval_secs`), draining a local write queue (`CalMutation`) before each sync pass. A write hitting a conflict is marked `db::CAL_STATUS_CONFLICT`, never auto-resolved
- **caldav_server.rs** — Optional real inbound CalDAV (RFC 4791) + WebDAV (RFC 4918) HTTP server hosted inside `jamaild` (off unless `daemon.caldav_server_listen` is set), exposing one auto-created "Default" calendar per `caldav`-configured account (`calendar_url = "server:<account>"`, a namespace distinct from any remote-synced calendar) — Basic auth against that account's own `caldav.login`/`caldav.auth`, PROPFIND discovery, REPORT `calendar-query`/`sync-collection`, and GET/PUT/DELETE with `If-Match`/`If-None-Match` ETag conflict protection through the same `db.rs` persistence mail/calendar data already uses. The mirror image of `caldav.rs` (client role vs. server role); both can be active for the same account at once
- **calnotify.rs** — Calendar alarm scheduling (`compute_alarms`/`due_now`, pure) and delivery (`AlarmSink` trait; `DesktopAlarmSink` is the real `notify-send`-based implementation). Unlike `notify.rs`'s silent best-effort mail notifications, alarm delivery failures are surfaced as `Result::Err`, not swallowed — see module doc for why, and for why alarms only fire while `jacal` is running (no always-on piece for calendars in this build)
- **calapp.rs** — `jacal`'s UI state and rendering (mirrors `app.rs`'s role for `jamail`): day/3-day/week/month/year views rendered as colored table grids (not a flat agenda list), with distinctly-colored weekend columns, a configurable week start (`config::WeekStart`), 2D focused-day/event-cursor navigation, calendar visibility toggles, and the create/edit/delete form (`DraftEvent`). `expand_recurring_events` turns each recurring DB row into one row per occurrence in the loaded window (via `calendar::expand_occurrences`) before display; all-day events get a distinct background and are shown on every day they span. State/navigation/form-validation/expansion logic is unit-tested; rendering is not (same convention as `app.rs`)

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
- **Folder display order matches sync order** — `mail::order_folders()` is the single source of truth for both what each account's sync thread in `jamaild` syncs (`SyncControl.folder_filter`) and what `jamail`'s folder selector shows (`App.rebuild_folder_tree`, driven by `ipc::Event::FoldersLoaded`), keyed off the same `accounts.<name>.folders` config. Unconfigured: INBOX first, then alphabetical (deterministic default, since IMAP LIST order and DB-cache order aren't guaranteed to agree or stay stable). When `folders` is set, `accounts.<name>.show_unlisted_folders` (default `false`) controls whether remote folders absent from that list are appended after it (same INBOX-first/alphabetical order) instead of being hidden/unsynced entirely.
- **Folder-selection cursor survives tree rebuilds** — `App.rebuild_folder_tree()` captures the currently-selected row's identity (account+folder name, or the virtual/global-inbox/account sentinel) before rebuilding, then relocates the cursor to that same row afterward regardless of index shifts from reordering, expand/collapse, or newly-synced folders; falls back to the first row when that identity is no longer present (row removed, account collapsed). `App.enter_folder_select()` relies on this instead of resetting the cursor, so reopening the folder selector returns to where you left it.
- **Remote Sent/Draft folder upload is opt-in and one-shot** — `sent_folder`/`draft_folder` are independent `Option<String>` keys; unset means "local cache only" (no APPEND attempted). A sent message is uploaded once (after a successful SMTP send); a draft is uploaded once, on its *first* explicit `Ctrl+S` save only — later edits update the local cache but do not re-upload, so there's no remote delete/replace-on-update logic to track a remote UID. `jamail` sends the raw message (base64'd) to `jamaild` as `ipc::Request::EnqueueUpload`, which pushes onto that account's `SyncControl.upload_queue`, drained by its sync thread (which already owns a live IMAP connection) alongside `mark_seen_queue`; results come back as `ipc::Event::UploadComplete`/`UploadError`.
- **Sent/Draft in-flight state never disappears silently** — `local_messages.status` includes `sending` and `send_error` (not just `draft`/`sent`), and `get_drafts()` includes all three non-`sent` states so a message stays visible (with a "Sending…"/"Send failed: ..." badge) through the whole send lifecycle. `upload_status`/`upload_error` track the independent Sent/Draft-folder upload outcome the same way. `Email.status_label` (set only for local Drafts/Sent rows) carries the badge text into both the flat and threaded list renderers.
- **Desktop notifications are owned entirely by `jamaild`** — `notify_folders` (optional, per account) lists folders that trigger a `notify-send` desktop notification + best-effort sound (`canberra-gtk-play`/`paplay`/`pw-play`) on new mail, mirroring the existing external-tool-with-graceful-fallback pattern (w3m, xdg-open, wl-copy/xclip/xsel). Unset means no notifications. The decision and the call both happen in `daemon::dispatch_sync_event`, run from each account's event-pump thread inside `jamaild` — never in `jamail`, and never gated on any client being connected, which is what makes this work under `systemd --user` with no terminal open at all. Fires once per `FolderComplete` sync event (not per message), so an initial full backfill sync produces one notification with the total count rather than one per message.

### Adding Keyboard Shortcuts

1. Add the key handler in `src/bin/jamail.rs` under the appropriate `ViewMode` match arm
2. Add the action method in `app.rs` on `App`
3. Update `render_status_bar()` in app.rs to show the new key hint

### External Tool Dependencies

- **w3m** — HTML-to-text conversion (optional; falls back to tag stripping)
- **xdg-open** — Opening HTML email in browser (`v` key)
- **wl-copy / xclip / xsel** — Clipboard copy for mouse selection (tried in order)
