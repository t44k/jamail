//! Best-effort desktop notification + sound for configured notify-trigger
//! folders. Uses the same external-tool-with-graceful-fallback pattern as
//! clipboard copy (`wl-copy`/`xclip`/`xsel`) and HTML preview (`xdg-open`):
//! `notify-send` is the standard freedesktop mechanism and works across
//! Wayland compositors (including Hyprland/Omarchy setups). If the binary
//! isn't installed, the call silently no-ops — no error is surfaced to the
//! user.
use std::process::{Command, Stdio};

/// Whether new mail in `folder` should trigger a notification for an
/// account, given its configured `notify_folders` (`None` = disabled).
pub fn should_notify(configured: Option<&[String]>, folder: &str) -> bool {
    match configured {
        Some(folders) => folders.iter().any(|f| f == folder),
        None => false,
    }
}

/// Build the (summary, body) text for a new-mail notification. Pure and
/// deterministic so it can be tested without spawning any process.
pub fn notification_text(account_label: &str, folder: &str, count: usize) -> (String, String) {
    let summary = format!("New mail — {}", account_label);
    let body = if count == 1 {
        format!("1 new message in {}", folder)
    } else {
        format!("{} new messages in {}", count, folder)
    };
    (summary, body)
}

/// Fire a desktop notification + unobtrusive sound for new mail. Both
/// steps are best-effort: missing binaries are silently ignored.
pub fn notify_new_mail(account_label: &str, folder: &str, count: usize) {
    let (summary, body) = notification_text(account_label, folder, count);
    send_desktop_notification(&summary, &body);
    play_notification_sound();
}

fn send_desktop_notification(summary: &str, body: &str) {
    let _ = Command::new("notify-send")
        .args(["-a", "jamail", "-i", "mail-message-new", summary, body])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Try a short list of common notification-sound players, in order, and
/// stop at the first one that's installed. Mirrors the clipboard-copy
/// fallback chain in main.rs.
fn play_notification_sound() {
    const CANDIDATES: &[(&str, &[&str])] = &[
        ("canberra-gtk-play", &["-i", "message-new-email"]),
        (
            "paplay",
            &["/usr/share/sounds/freedesktop/stereo/message-new-instant.oga"],
        ),
        (
            "pw-play",
            &["/usr/share/sounds/freedesktop/stereo/message-new-instant.oga"],
        ),
    ];

    for (cmd, args) in CANDIDATES {
        if Command::new(cmd)
            .args(*args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok()
        {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_notify_is_disabled_when_unconfigured() {
        assert!(!should_notify(None, "INBOX"));
    }

    #[test]
    fn should_notify_matches_configured_folder_only() {
        let folders = vec!["INBOX".to_string(), "Important".to_string()];
        assert!(should_notify(Some(&folders), "INBOX"));
        assert!(should_notify(Some(&folders), "Important"));
        assert!(!should_notify(Some(&folders), "Archive"));
    }

    #[test]
    fn should_notify_with_explicit_empty_list_never_fires() {
        let folders: Vec<String> = vec![];
        assert!(!should_notify(Some(&folders), "INBOX"));
    }

    #[test]
    fn notification_text_singular_vs_plural_count() {
        let (summary, body) = notification_text("Alice", "INBOX", 1);
        assert_eq!(summary, "New mail — Alice");
        assert_eq!(body, "1 new message in INBOX");

        let (_, body) = notification_text("Alice", "INBOX", 3);
        assert_eq!(body, "3 new messages in INBOX");
    }

    #[test]
    fn notify_new_mail_does_not_panic_when_tools_are_missing() {
        // Best-effort spawn: must never panic or error out even in a bare
        // test/CI environment with no notify-send/canberra/paplay/pw-play.
        notify_new_mail("Alice", "INBOX", 2);
    }
}
