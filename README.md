# jamail

A terminal email client built with Rust. Connects to IMAP servers, caches everything locally in a compressed SQLite database, and provides instant full-text search.

jamail ships as two binaries: **`jamaild`**, a background daemon that owns every account's IMAP connection and delivers desktop notifications (suitable for `systemd --user`, keeps working even with no terminal open), and **`jamail`**, the terminal UI, which talks to it over a small local IPC protocol. See [Architecture: jamaild + jamail](#architecture-jamaild--jamail) below.

## Features

- **Multiple IMAP accounts**, all synced continuously by a background daemon, with configurable, deterministic folder filtering/ordering
- **Compose, reply, and forward** emails via SMTP, with multiple sender identities per account
- **Optional Sent/Draft folder upload** — keep a remote copy of sent mail and drafts, independently configurable, with clear in-flight/error indicators
- **Desktop notifications** (+ unobtrusive sound) for configurable new-mail-trigger folders — delivered by the daemon, so they keep working whether or not the terminal UI is open
- **Contact autocomplete** from known addresses in your mail cache
- **File attachments** via built-in file browser
- **Local SQLite cache** with zstd compression — emails are available offline and load instantly
- **Full-text search** via FTS5
- **Email threading** using Message-ID/In-Reply-To/References headers
- **Background sync** with IMAP IDLE for real-time updates
- **Attachment support** — view metadata, save to disk, image previews in supported terminals
- **Mouse text selection** with automatic clipboard copy
- **HTML email rendering** via w3m (falls back to tag stripping)

## Install

```bash
cargo install --path .
```

Requires Rust edition 2024 (nightly or stable 1.85+).

### Build dependencies

jamail links against the system OpenSSL (via `native-tls`, used for IMAP and SMTP TLS), so
`pkg-config` and the OpenSSL development headers must be present before `cargo build`. On
Debian/Ubuntu:

```bash
sudo apt-get install -y pkg-config libssl-dev
```

Equivalents: `sudo dnf install pkg-config openssl-devel` (Fedora/RHEL),
`sudo pacman -S pkgconf openssl` (Arch), `sudo apk add pkgconf openssl-dev` (Alpine).
Without them the build fails in `openssl-sys` with a "could not find system library
'openssl'" error.

`cargo install --path .` installs both `jamail` and `jamaild` (to `~/.cargo/bin` by
default).

## Architecture: jamaild + jamail

- **`jamaild`** is a background daemon. On start it loads `config.yaml` and begins
  syncing *every* configured account concurrently over IMAP (not just whichever one
  you're currently looking at) — that's what lets it keep noticing new mail and
  firing desktop notifications even when no `jamail` terminal is open. It listens on
  a Unix domain socket (see below) and writes to the same local SQLite cache
  `jamail` reads from.
- **`jamail`** is the terminal UI. It reads the cache directly and talks to
  `jamaild` over that socket for anything that needs a live IMAP session:
  which folder to prioritize for IMAP IDLE, marking messages seen, and
  Sent/Draft folder uploads.
- If you just run `jamail` and no `jamaild` is reachable, `jamail` spawns one for
  you automatically (detached, so it outlives the terminal). This means a bare
  `jamail` still "just works" with no extra setup — the daemon is the one thing
  actually talking to your mail server, `jamail` is a disposable view onto it.
- For notifications that work even when you never open `jamail` at all, run
  `jamaild` as a persistent `systemd --user` service instead of relying on
  auto-start: see [`contrib/systemd/jamaild.service`](contrib/systemd/jamaild.service).

  ```bash
  mkdir -p ~/.config/systemd/user
  cp contrib/systemd/jamaild.service ~/.config/systemd/user/
  systemctl --user daemon-reload
  systemctl --user enable --now jamaild
  ```

  The unit's `ExecStart` assumes `cargo install --path .`'s default location
  (`~/.cargo/bin/jamaild`) — edit it if you installed elsewhere (`which jamaild`).
  `systemctl --user stop jamaild` (or plain `Ctrl+C` on a foreground `jamaild`)
  shuts it down gracefully: every account's sync loop stops and the socket file
  is removed before it exits.

### Socket path

`jamaild` listens on, and `jamail` connects to, in this order:

1. the `JAMAIL_SOCKET` environment variable, if set;
2. `accounts.daemon.socket_path` in `config.yaml`, if set;
3. `$XDG_RUNTIME_DIR/jamail/jamaild.sock` (the normal case under any modern
   Linux desktop or systemd --user session);
4. `/tmp/jamail-<uid>/jamaild.sock` if `XDG_RUNTIME_DIR` isn't set.

The socket's containing directory is created `0700` and the socket file itself is
`chmod`'d `0600` right after binding — only your own user can connect, the same
trust model as `ssh-agent`/`gpg-agent`. The full wire protocol (framing, versioning,
error semantics) is documented in `src/ipc.rs`.

### Optional dependencies

| Tool | Purpose | Without it |
|------|---------|------------|
| w3m | HTML-to-text rendering | Falls back to basic tag stripping |
| xdg-open | Open HTML emails in browser (`v` key) | Feature unavailable |
| wl-copy / xclip / xsel | Clipboard for mouse selection | Selection won't copy |
| notify-send | Desktop notification for `notify_folders` | Notification silently skipped |
| canberra-gtk-play / paplay / pw-play | Notification sound | Sound silently skipped |

## Configuration

Create `~/.config/jamail/config.yaml`:

```yaml
accounts:
  personal:
    default: true
    email: alice@example.com
    display_name: Alice
    imap:
      host: imap.example.com
      port: 993
      login: alice@example.com
      auth:
        type: password
        value: my-secret-password
    smtp:
      host: smtp.example.com
      port: 587
      login: alice@example.com
      auth:
        type: password
        value: my-secret-password
      starttls: true

  work:
    email: alice@corp.com
    imap:
      host: imap.corp.com
      port: 993
      login: alice@corp.com
      auth:
        type: command
        value: pass show email/work
    smtp:
      host: smtp.corp.com
      port: 587
      login: alice@corp.com
      auth:
        type: command
        value: pass show email/work
      starttls: true
    folders:
      - INBOX
      - Sent
      - Drafts
```

### Config fields

| Field | Required | Description |
|-------|----------|-------------|
| `accounts.<name>.default` | No | Mark one account as default (first account used otherwise) |
| `accounts.<name>.email` | Yes | Email address |
| `accounts.<name>.display_name` | No | Display name |
| `accounts.<name>.imap.host` | Yes | IMAP server hostname |
| `accounts.<name>.imap.port` | Yes | IMAP port (993 for TLS) |
| `accounts.<name>.imap.login` | Yes | IMAP login username |
| `accounts.<name>.imap.auth.type` | Yes | `password` or `command` |
| `accounts.<name>.imap.auth.value` | Yes | Literal password, or shell command that outputs it |
| `accounts.<name>.folders` | No | Ordered list of folders to sync **and** to show in the folder selector (`F`). Omitted: all server folders are synced/shown, deterministically ordered INBOX-first then alphabetical (instead of whatever order the server/cache happens to return) |
| `accounts.<name>.show_unlisted_folders` | No | When `folders` is set, also sync/show remote folders not listed there, appended after the configured ones (INBOX-first, then alphabetical). Omitted (default `false`): only the folders listed in `folders` are synced/shown. No effect if `folders` is unset |
| `accounts.<name>.senders` | No | Additional "From" identities, cycled with `Left`/`Right` on the From field in compose. Omitted: falls back to a single identity built from `display_name`/`email`. The selected sender is always validated against this list before sending |
| `accounts.<name>.sent_folder` | No | IMAP folder to upload a copy of successfully-sent messages to. Omitted (default): sent messages stay in the local cache only — nothing is uploaded |
| `accounts.<name>.draft_folder` | No | IMAP folder to upload a newly-created draft to (on its first explicit save with `Ctrl+S`). Omitted (default): drafts stay in the local cache only — nothing is uploaded |
| `accounts.<name>.notify_folders` | No | Folders that trigger a desktop notification + sound when new mail arrives. Omitted (default): no notifications for this account |
| `accounts.<name>.smtp.host` | No | SMTP server hostname (required for sending) |
| `accounts.<name>.smtp.port` | No | SMTP port (587 for STARTTLS, 465 for implicit TLS) |
| `accounts.<name>.smtp.login` | No | SMTP login username |
| `accounts.<name>.smtp.auth.type` | No | `password` or `command` (same as IMAP auth) |
| `accounts.<name>.smtp.auth.value` | No | Literal password, or shell command that outputs it |
| `accounts.<name>.smtp.starttls` | No | `true` for STARTTLS (default), `false` for implicit TLS |
| `daemon.socket_path` | No | Override the `jamaild`/`jamail` IPC socket path (see [Socket path](#socket-path)). Omitted (default): `$XDG_RUNTIME_DIR/jamail/jamaild.sock` |

Auth type `command` runs the value as a shell command and reads the password from stdout. Works with `pass`, `gpg`, `secret-tool`, etc.

The `smtp` section is optional — existing read-only configs continue to work. Without it, compose/reply/forward keys are available but sending will show an error.

### Sent/Draft upload and status indicators

By default, sent messages and drafts are cached locally only — jamail never uploads anything to the server unless `sent_folder`/`draft_folder` are set. Each is independent: you can configure one without the other.

The Drafts/Sent list always shows what's actually happening to a message, so nothing silently disappears:

| Badge | Meaning |
|-------|---------|
| `Sending…` | Queued and being sent in the background |
| `Send failed: ...` | The last send attempt failed; the message stays in Drafts so you can retry |
| `Uploading to Sent…` / `Uploading draft…` | Sent/saved locally, now being APPENDed to the configured remote folder |
| `Uploaded` / `Draft uploaded` | Remote upload succeeded |
| `Upload failed: ...` / `Draft upload failed: ...` | Remote upload failed (the message itself was still sent/saved locally) |

Drafts are uploaded once, on their first explicit save — later edits update the local cache but don't re-upload, so repeated `Ctrl+S` saves don't pile up duplicate copies in the remote Drafts folder.

### Desktop notifications

Set `notify_folders` on an account to get a desktop notification + short sound when new mail arrives in one of those folders. This uses `notify-send` (works across Wayland compositors, including Hyprland/Omarchy setups) for the notification and tries `canberra-gtk-play`, then `paplay`, then `pw-play` for the sound. If none of those are installed, notifications/sound are silently skipped — no error, no crash.

Notification delivery is entirely `jamaild`'s responsibility: it decides whether to
notify and fires the notification itself, for every configured account, regardless
of whether `jamail` is open or which account/folder it's showing. Run `jamaild` as a
`systemd --user` service (see above) to get notifications with no terminal open at
all.

## Usage

```bash
jamail
```

The database is stored at `~/.local/share/jamail/mail.db` and acts as a cache — delete it anytime to force a full re-sync. `jamail` auto-starts a `jamaild` if none is reachable; see [Architecture: jamaild + jamail](#architecture-jamaild--jamail) for the recommended `systemd --user` setup instead.

### Keyboard shortcuts

#### List view
| Key | Action |
|-----|--------|
| `Up/Down` | Navigate emails |
| `Enter` | Open email |
| `n` | Compose new email |
| `Space` / `l` | Toggle thread expand/collapse |
| `Tab` / `Shift+Tab` | Next / previous thread |
| `t` | Toggle thread mode |
| `/` | Search |
| `F` | Folder selector |
| `g` / `G` | Jump to top / bottom |
| `q` | Quit |

#### Detail view
| Key | Action |
|-----|--------|
| `Up/Down` | Scroll |
| `Space` / `PageDown/Up` | Page scroll |
| `Left/Right` | Previous / next email |
| `r` | Reply to email |
| `f` | Forward email |
| `n` / `p` | Next / previous in thread |
| `h` | Toggle raw headers |
| `v` | Open HTML in browser |
| `/` | Cycle mode: Text → Attachments → Links |
| `s` | Save attachment (in Attachments mode) |
| `Enter` | Open link (in Links mode) |
| `Esc` | Back to list |

#### Compose view
| Key | Action |
|-----|--------|
| `Ctrl+X` | Send email (`Ctrl+Enter` isn't reliably detectable on most terminals) |
| `Left` / `Right` (on From field) | Cycle sender identity (only shown when `senders` has more than one entry) |
| `Tab` / `Shift+Tab` | Next / previous field |
| `Ctrl+A` | Open file browser to attach files |
| `Ctrl+D` | Remove last attachment |
| `Ctrl+S` | Save as draft (uploads to `draft_folder` on first save, if configured) |
| `Esc` | Cancel compose |

In address fields (To, Cc, Bcc), autocomplete suggestions appear as you type. Use `Up/Down` to navigate suggestions and `Tab` or `Enter` to accept.

The file browser supports `Up/Down` to navigate, `Enter` to select/descend, `Backspace` to go to parent directory, and `.` to toggle hidden files.

#### Search view
| Key | Action |
|-----|--------|
| Type | Enter FTS5 query |
| `Up/Down` | Navigate results |
| `Enter` | Open email / execute search |
| `Esc` | Exit search |

Mouse scroll works in all views. Click and drag in detail view to select text (auto-copied to clipboard).

## License

MIT
