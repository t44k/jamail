//! Calendar alarm notification scheduling and delivery, for `jacal`.
//!
//! ## Ownership: why this differs from mail's `notify` module
//!
//! Mail notifications (`notify.rs`) are owned entirely by `jamaild`,
//! fire-and-forget, and silently no-op when `notify-send`/a sound player
//! isn't installed — that's the right call there because `jamaild` is
//! meant to run unattended under `systemd --user`, with nothing watching
//! its output, so a failed notification attempt has nowhere useful to go
//! and shouldn't interrupt sync.
//!
//! Calendar alarms are different in two ways this module's design follows
//! directly:
//!
//! 1. **Delivery lives in `jacal`, not `jamaild`.** `jamaild` syncs
//!    calendar data into the shared cache, but nothing in this codebase
//!    keeps `jacal` itself running unattended (unlike `jamaild`, there is
//!    no `contrib/systemd` unit for it) — so unlike mail, a calendar alarm
//!    only fires while `jacal` happens to be open. This is a real,
//!    explicitly documented limitation, not an oversight: making calendar
//!    alarms as reliable as mail notifications would mean either teaching
//!    `jamaild` to own a scheduling clock for every account's events (a
//!    much bigger change than this task's scope) or shipping a second
//!    always-on daemon. Neither was justified for a first cut.
//! 2. **Failures are surfaced, not swallowed.** [`AlarmSink::deliver`]
//!    returns `Result<(), String>`, and [`AlarmScheduler::tick`] returns
//!    every delivery attempt's outcome rather than discarding it — `jacal`
//!    shows a failed alarm delivery in its status bar instead of pretending
//!    it fired. A calendar reminder silently vanishing (no popup, no
//!    indication anything was even attempted) is a materially worse
//!    failure mode for "you have a meeting in 5 minutes" than for "you
//!    have new mail", which is why this module doesn't reuse `notify.rs`'s
//!    silent-best-effort pattern.
//!
//! ## Scheduling
//!
//! [`compute_alarms`] is pure: given a set of events (already reconstructed
//! as [`crate::calendar::VEvent`], typically via
//! [`crate::db::CalendarEventRow::to_vevent`]) and a fallback lead time
//! (from [`crate::config::CalDavConfig::default_alarm_minutes_before`]),
//! it returns every [`AlarmFire`] — one per `VALARM` on events that have
//! one, or one per configured default lead time on events that don't.
//! Cancelled events produce no alarms.
//!
//! [`AlarmScheduler`] adds the stateful part: which alarms have already
//! fired this session (so a restart-free `jacal` doesn't re-notify every
//! tick) and a "how late is too late" grace window (so a `jacal` that was
//! closed when an alarm's trigger time passed doesn't fire a stale
//! notification for a meeting that already started an hour ago on
//! restart).

use crate::calendar::{EventStatus, VEvent};
use chrono::{DateTime, Duration, Local, Utc};
use std::collections::HashSet;
use std::process::{Command, Stdio};

/// A single alarm that should fire, resolved to a concrete instant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlarmFire {
    pub uid: String,
    pub summary: String,
    pub location: String,
    pub fire_at: DateTime<Utc>,
    pub event_start: DateTime<Utc>,
}

impl AlarmFire {
    /// Build the (summary, body) text for a desktop notification. Pure and
    /// deterministic so it's testable without spawning any process (same
    /// pattern as `notify::notification_text`).
    pub fn notification_text(&self) -> (String, String) {
        let when = self.event_start.with_timezone(&Local).format("%H:%M");
        let body = if self.location.is_empty() {
            format!("Starting at {}", when)
        } else {
            format!("{} — starting at {}", self.location, when)
        };
        (self.summary.clone(), body)
    }
}

/// Delivery abstraction for a due alarm. See module docs for why this
/// returns `Result` (explicit failure) rather than following `notify.rs`'s
/// silent best-effort pattern.
pub trait AlarmSink {
    fn deliver(&self, fire: &AlarmFire) -> Result<(), String>;
}

/// The real sink: `notify-send` (+ best-effort sound, same player fallback
/// chain as `notify.rs`). A failure to even *launch* `notify-send` (e.g.
/// not installed) is reported as `Err`, not swallowed — that's the one
/// concrete behavioral difference from `notify::notify_new_mail`. A sound
/// player failing is still best-effort (it's a secondary cue; the visual
/// notification having gone out is what matters for "was this delivered").
pub struct DesktopAlarmSink;

impl AlarmSink for DesktopAlarmSink {
    fn deliver(&self, fire: &AlarmFire) -> Result<(), String> {
        let (summary, body) = fire.notification_text();
        Command::new("notify-send")
            .args(["-a", "jacal", "-i", "appointment-soon", &summary, &body])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("notify-send failed to start: {}", e))?;
        play_alarm_sound();
        Ok(())
    }
}

fn play_alarm_sound() {
    const CANDIDATES: &[(&str, &[&str])] = &[
        ("canberra-gtk-play", &["-i", "alarm-clock-elapsed"]),
        (
            "paplay",
            &["/usr/share/sounds/freedesktop/stereo/alarm-clock-elapsed.oga"],
        ),
        (
            "pw-play",
            &["/usr/share/sounds/freedesktop/stereo/alarm-clock-elapsed.oga"],
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

/// Compute every alarm that fires for `events`: one per `VALARM` when an
/// event has any, otherwise one per entry in `default_lead_minutes`
/// (interpreted as minutes before `DTSTART`). Cancelled events are
/// skipped entirely.
pub fn compute_alarms(events: &[VEvent], default_lead_minutes: &[i64]) -> Vec<AlarmFire> {
    let mut out = Vec::new();
    for e in events {
        if e.status == EventStatus::Cancelled {
            continue;
        }
        if e.alarms.is_empty() {
            for &minutes in default_lead_minutes {
                out.push(AlarmFire {
                    uid: e.uid.clone(),
                    summary: e.summary.clone(),
                    location: e.location.clone(),
                    fire_at: e.dtstart.utc - Duration::minutes(minutes),
                    event_start: e.dtstart.utc,
                });
            }
        } else {
            for alarm in &e.alarms {
                out.push(AlarmFire {
                    uid: e.uid.clone(),
                    summary: e.summary.clone(),
                    location: e.location.clone(),
                    fire_at: alarm.trigger.fire_time(e.dtstart.utc, e.dtend.utc),
                    event_start: e.dtstart.utc,
                });
            }
        }
    }
    out
}

/// Alarms from `all` whose trigger time has passed but not by more than
/// `grace` — due "now", within a bounded look-back window so a `jacal`
/// that was closed past an alarm's trigger time doesn't fire a stale
/// notification for a meeting long over by the time it's reopened.
pub fn due_now(all: &[AlarmFire], now: DateTime<Utc>, grace: Duration) -> Vec<&AlarmFire> {
    all.iter()
        .filter(|a| a.fire_at <= now && now - a.fire_at <= grace)
        .collect()
}

/// Default look-back window for [`AlarmScheduler::tick`]: an alarm whose
/// trigger time is more than this far in the past when first observed is
/// treated as missed, not fired late.
pub const DEFAULT_GRACE: Duration = Duration::minutes(2);

/// Stateful wrapper around [`compute_alarms`]/[`due_now`] that tracks which
/// `(uid, fire_at)` pairs have already been delivered this session, so a
/// repeated call with the same events doesn't re-notify. Session-local
/// only — state resets on `jacal` restart (see module docs).
pub struct AlarmScheduler {
    fired: HashSet<(String, i64)>,
    grace: Duration,
}

impl AlarmScheduler {
    pub fn new() -> Self {
        Self {
            fired: HashSet::new(),
            grace: DEFAULT_GRACE,
        }
    }

    #[cfg(test)]
    fn with_grace(grace: Duration) -> Self {
        Self {
            fired: HashSet::new(),
            grace,
        }
    }

    /// Compute due alarms for `events` as of `now`, deliver each new one
    /// via `sink`, and return every delivery attempt's outcome (never
    /// silently dropped — see module docs). Already-delivered alarms are
    /// skipped without calling `sink` again.
    pub fn tick(
        &mut self,
        events: &[VEvent],
        default_lead_minutes: &[i64],
        now: DateTime<Utc>,
        sink: &dyn AlarmSink,
    ) -> Vec<(AlarmFire, Result<(), String>)> {
        let all = compute_alarms(events, default_lead_minutes);
        let due = due_now(&all, now, self.grace);
        let mut results = Vec::new();
        for fire in due {
            let key = (fire.uid.clone(), fire.fire_at.timestamp());
            if !self.fired.insert(key) {
                continue;
            }
            let outcome = sink.deliver(fire);
            results.push((fire.clone(), outcome));
        }
        results
    }
}

impl Default for AlarmScheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::parse_vevents;

    fn event_with_valarm(uid: &str, minutes_before: i64) -> VEvent {
        parse_vevents(&format!(
            "BEGIN:VEVENT\r\nUID:{uid}\r\nDTSTART:20240115T090000Z\r\nDTEND:20240115T100000Z\r\nSUMMARY:Meeting\r\nLOCATION:Room A\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT{m}M\r\nEND:VALARM\r\nEND:VEVENT\r\n",
            uid = uid,
            m = minutes_before
        ))
        .unwrap()
        .remove(0)
    }

    fn event_without_alarm(uid: &str) -> VEvent {
        parse_vevents(&format!(
            "BEGIN:VEVENT\r\nUID:{}\r\nDTSTART:20240115T090000Z\r\nDTEND:20240115T100000Z\r\nSUMMARY:No alarm\r\nEND:VEVENT\r\n",
            uid
        ))
        .unwrap()
        .remove(0)
    }

    fn cancelled_event(uid: &str) -> VEvent {
        parse_vevents(&format!(
            "BEGIN:VEVENT\r\nUID:{}\r\nDTSTART:20240115T090000Z\r\nSTATUS:CANCELLED\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT15M\r\nEND:VALARM\r\nEND:VEVENT\r\n",
            uid
        ))
        .unwrap()
        .remove(0)
    }

    #[test]
    fn compute_alarms_uses_explicit_valarm_when_present() {
        let events = vec![event_with_valarm("e1", 15)];
        let alarms = compute_alarms(&events, &[30]);
        assert_eq!(alarms.len(), 1);
        assert_eq!(alarms[0].uid, "e1");
        // 09:00 - 15min = 08:45
        assert_eq!(
            alarms[0].fire_at,
            "2024-01-15T08:45:00Z".parse::<DateTime<Utc>>().unwrap()
        );
    }

    #[test]
    fn compute_alarms_falls_back_to_default_lead_times_when_no_valarm() {
        let events = vec![event_without_alarm("e2")];
        let alarms = compute_alarms(&events, &[30, 5]);
        assert_eq!(alarms.len(), 2);
        assert!(alarms.iter().all(|a| a.uid == "e2"));
        let mut fire_times: Vec<_> = alarms.iter().map(|a| a.fire_at).collect();
        fire_times.sort();
        assert_eq!(
            fire_times[0],
            "2024-01-15T08:30:00Z".parse::<DateTime<Utc>>().unwrap()
        );
        assert_eq!(
            fire_times[1],
            "2024-01-15T08:55:00Z".parse::<DateTime<Utc>>().unwrap()
        );
    }

    #[test]
    fn compute_alarms_produces_nothing_for_events_with_no_valarm_and_no_default() {
        let events = vec![event_without_alarm("e3")];
        let alarms = compute_alarms(&events, &[]);
        assert!(alarms.is_empty());
    }

    #[test]
    fn compute_alarms_skips_cancelled_events_entirely() {
        let events = vec![cancelled_event("e4")];
        let alarms = compute_alarms(&events, &[30]);
        assert!(alarms.is_empty());
    }

    #[test]
    fn due_now_includes_past_due_within_grace_excludes_future_and_too_old() {
        let now: DateTime<Utc> = "2024-01-15T09:00:00Z".parse().unwrap();
        let alarms = vec![
            AlarmFire {
                uid: "future".to_string(),
                summary: "".to_string(),
                location: "".to_string(),
                fire_at: now + Duration::minutes(5),
                event_start: now,
            },
            AlarmFire {
                uid: "just-due".to_string(),
                summary: "".to_string(),
                location: "".to_string(),
                fire_at: now - Duration::seconds(30),
                event_start: now,
            },
            AlarmFire {
                uid: "too-old".to_string(),
                summary: "".to_string(),
                location: "".to_string(),
                fire_at: now - Duration::minutes(10),
                event_start: now,
            },
        ];
        let due = due_now(&alarms, now, Duration::minutes(2));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].uid, "just-due");
    }

    struct RecordingSink {
        calls: std::sync::Mutex<Vec<String>>,
    }
    impl AlarmSink for RecordingSink {
        fn deliver(&self, fire: &AlarmFire) -> Result<(), String> {
            self.calls.lock().unwrap().push(fire.uid.clone());
            Ok(())
        }
    }

    struct FailingSink;
    impl AlarmSink for FailingSink {
        fn deliver(&self, _fire: &AlarmFire) -> Result<(), String> {
            Err("simulated delivery failure".to_string())
        }
    }

    #[test]
    fn scheduler_does_not_redeliver_the_same_alarm_twice() {
        let mut scheduler = AlarmScheduler::with_grace(Duration::minutes(5));
        let events = vec![event_with_valarm("e5", 15)];
        let now: DateTime<Utc> = "2024-01-15T08:45:00Z".parse().unwrap();
        let sink = RecordingSink {
            calls: std::sync::Mutex::new(Vec::new()),
        };

        let first = scheduler.tick(&events, &[], now, &sink);
        assert_eq!(first.len(), 1);
        let second = scheduler.tick(&events, &[], now, &sink);
        assert!(
            second.is_empty(),
            "must not redeliver an already-fired alarm"
        );
        assert_eq!(*sink.calls.lock().unwrap(), vec!["e5".to_string()]);
    }

    #[test]
    fn scheduler_surfaces_delivery_failures_rather_than_swallowing_them() {
        let mut scheduler = AlarmScheduler::with_grace(Duration::minutes(5));
        let events = vec![event_with_valarm("e6", 15)];
        let now: DateTime<Utc> = "2024-01-15T08:45:00Z".parse().unwrap();
        let results = scheduler.tick(&events, &[], now, &FailingSink);
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].1.as_ref().unwrap_err(),
            "simulated delivery failure"
        );
    }

    #[test]
    fn notification_text_includes_location_when_present() {
        let fire = AlarmFire {
            uid: "x".to_string(),
            summary: "Standup".to_string(),
            location: "Room A".to_string(),
            fire_at: Utc::now(),
            event_start: "2024-01-15T09:00:00Z".parse().unwrap(),
        };
        let (summary, body) = fire.notification_text();
        assert_eq!(summary, "Standup");
        assert!(body.contains("Room A"));
    }

    #[test]
    fn notification_text_omits_location_when_absent() {
        let fire = AlarmFire {
            uid: "x".to_string(),
            summary: "Standup".to_string(),
            location: "".to_string(),
            fire_at: Utc::now(),
            event_start: "2024-01-15T09:00:00Z".parse().unwrap(),
        };
        let (_, body) = fire.notification_text();
        assert!(body.starts_with("Starting at"));
    }

    #[test]
    fn desktop_alarm_sink_reports_explicit_failure_when_notify_send_is_missing() {
        // This sandboxed test environment has no notify-send installed
        // (verified during development) — exercising the real spawn path
        // here proves the explicit-failure contract end-to-end rather than
        // just asserting against a mock. If some environment *does* have
        // notify-send installed, this assertion would need updating; that
        // trade-off is accepted for the stronger guarantee it gives
        // everywhere else.
        let fire = AlarmFire {
            uid: "x".to_string(),
            summary: "Test".to_string(),
            location: "".to_string(),
            fire_at: Utc::now(),
            event_start: Utc::now(),
        };
        let sink = DesktopAlarmSink;
        let result = sink.deliver(&fire);
        if which_notify_send_exists() {
            // Environment actually has notify-send: just assert it
            // doesn't panic either way.
        } else {
            assert!(result.is_err());
        }
    }

    fn which_notify_send_exists() -> bool {
        std::process::Command::new("sh")
            .args(["-c", "command -v notify-send"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}
