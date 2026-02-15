# jamail

A terminal email client built with Rust. Connects to IMAP servers, caches everything locally in a compressed SQLite database, and provides instant full-text search.

## Features

- **Multiple IMAP accounts** with configurable folder filtering
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

### Optional dependencies

| Tool | Purpose | Without it |
|------|---------|------------|
| w3m | HTML-to-text rendering | Falls back to basic tag stripping |
| xdg-open | Open HTML emails in browser (`v` key) | Feature unavailable |
| wl-copy / xclip / xsel | Clipboard for mouse selection | Selection won't copy |

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

  work:
    email: alice@corp.com
    imap:
      host: imap.corp.com
      port: 993
      login: alice@corp.com
      auth:
        type: command
        value: pass show email/work
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
| `accounts.<name>.folders` | No | Ordered list of folders to sync (all folders if omitted) |

Auth type `command` runs the value as a shell command and reads the password from stdout. Works with `pass`, `gpg`, `secret-tool`, etc.

## Usage

```bash
jamail
```

The database is stored at `~/.local/share/jamail/mail.db` and acts as a cache — delete it anytime to force a full re-sync.

### Keyboard shortcuts

#### List view
| Key | Action |
|-----|--------|
| `Up/Down` | Navigate emails |
| `Enter` | Open email |
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
| `n` / `p` | Next / previous in thread |
| `h` | Toggle raw headers |
| `v` | Open HTML in browser |
| `/` | Cycle mode: Text → Attachments → Links |
| `s` | Save attachment (in Attachments mode) |
| `Enter` | Open link (in Links mode) |
| `Esc` | Back to list |

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
