//! `jacal`'s TUI state and rendering: view navigation (year/month/week/
//! 3-day/day/single event, narrowed and widened one step at a time by
//! `Tab`/`Shift+Tab`), calendar selection, event list selection, the
//! create/edit/delete form state, and drawing all of it — including the
//! shared time grid the day-column views are laid out on (see
//! [`plan_time_axis`]) — mirrors `app.rs`'s role for
//! `jamail` (CLAUDE.md: "All UI state and rendering"). `bin/jacal.rs`
//! stays a thin event loop: it loads events from the database and sends
//! IPC requests, feeding the results into a [`CalApp`], the same split
//! `bin/jamail.rs`/[`crate::app::App`] use. Nothing in this module touches
//! `MailDb` or `ipc::IpcClient` directly — state/navigation/form-validation
//! logic is unit-tested below; rendering is not (same convention as
//! `app.rs`: TUI rendering itself isn't unit-tested in this crate).

use crate::calendar::{
    EventEdits, EventStatus, EventTime, VEvent, expand_occurrences, validate_rrule,
};
use crate::config::WeekStart;
use crate::db::{CalendarEventRow, CalendarRow};
use crate::theme;
use chrono::{
    DateTime, Datelike, Duration, Local, NaiveDate, NaiveTime, TimeZone, Timelike, Utc, Weekday,
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

/// The timeframes `jacal` can show, declared widest-first — the order
/// `Tab`/`Shift+Tab` walk. [`CalView::Event`] is the narrowest "range" of
/// all: a single event's detail.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CalView {
    Year,
    Month,
    Week,
    ThreeDay,
    Day,
    Event,
}

impl CalView {
    pub fn label(&self) -> &'static str {
        match self {
            CalView::Year => "Year",
            CalView::Month => "Month",
            CalView::Week => "Week",
            CalView::ThreeDay => "3-Day",
            CalView::Day => "Day",
            CalView::Event => "Event",
        }
    }

    /// One step narrower: Year -> Month -> Week -> 3-Day -> Day -> Event.
    /// `None` at the narrow end — deliberately saturating rather than
    /// wrapping, so `Tab` *always* steps into a smaller range and never
    /// jumps back out to Year.
    pub fn narrower(&self) -> Option<CalView> {
        match self {
            CalView::Year => Some(CalView::Month),
            CalView::Month => Some(CalView::Week),
            CalView::Week => Some(CalView::ThreeDay),
            CalView::ThreeDay => Some(CalView::Day),
            CalView::Day => Some(CalView::Event),
            CalView::Event => None,
        }
    }

    /// One step wider — the exact inverse of [`Self::narrower`], `None` at
    /// the wide end (`Shift+Tab` never wraps around to Event either).
    pub fn wider(&self) -> Option<CalView> {
        match self {
            CalView::Year => None,
            CalView::Month => Some(CalView::Year),
            CalView::Week => Some(CalView::Month),
            CalView::ThreeDay => Some(CalView::Week),
            CalView::Day => Some(CalView::ThreeDay),
            CalView::Event => Some(CalView::Day),
        }
    }

    /// True for the "columns of days" views (Day/3-Day/Week) and the
    /// single-event view, where Up/Down navigates *events within the
    /// focused day* and Left/Right moves the focused day itself. False for
    /// the grid views (Month/Year), where all four arrows move between
    /// grid cells.
    fn is_columnar(&self) -> bool {
        matches!(
            self,
            CalView::Day | CalView::ThreeDay | CalView::Week | CalView::Event
        )
    }
}

/// True for Saturday/Sunday, regardless of [`WeekStart`] — which day a
/// week visually starts on doesn't change which days are "the weekend".
fn is_weekend(d: NaiveDate) -> bool {
    matches!(d.weekday(), Weekday::Sat | Weekday::Sun)
}

/// The local calendar date a UTC timestamp falls on. Falls back to "now"
/// only for a corrupt/out-of-range timestamp, which should never occur for
/// data that round-tripped through [`crate::calendar`]'s parser.
fn local_date_of(ts_utc: i64) -> NaiveDate {
    DateTime::<Utc>::from_timestamp(ts_utc, 0)
        .unwrap_or_else(Utc::now)
        .with_timezone(&Local)
        .date_naive()
}

trait WeekStartExt {
    /// Days from this week-start convention's first day to `weekday`
    /// (0 for the start-of-week day itself, up to 6).
    fn offset(&self, weekday: Weekday) -> i64;
    /// Column headers, in display order, for this week-start convention.
    fn column_labels(&self) -> [&'static str; 7];
}

impl WeekStartExt for WeekStart {
    fn offset(&self, weekday: Weekday) -> i64 {
        match self {
            WeekStart::Monday => weekday.num_days_from_monday() as i64,
            WeekStart::Sunday => weekday.num_days_from_sunday() as i64,
        }
    }

    fn column_labels(&self) -> [&'static str; 7] {
        match self {
            WeekStart::Monday => ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"],
            WeekStart::Sunday => ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"],
        }
    }
}

fn start_of_week(d: NaiveDate, week_start: WeekStart) -> NaiveDate {
    d - Duration::days(week_start.offset(d.weekday()))
}

fn start_of_month(d: NaiveDate) -> NaiveDate {
    NaiveDate::from_ymd_opt(d.year(), d.month(), 1).unwrap()
}

/// Add `delta` months to `d`, always landing on day 1 of the resulting
/// month (used for month-view navigation, where the exact day within the
/// month isn't meaningful for the anchor).
fn add_months(d: NaiveDate, delta: i32) -> NaiveDate {
    let total = d.year() * 12 + (d.month() as i32 - 1) + delta;
    let year = total.div_euclid(12);
    let month0 = total.rem_euclid(12);
    NaiveDate::from_ymd_opt(year, (month0 + 1) as u32, 1).unwrap()
}

/// The full grid of calendar-day cells for the month containing `d`,
/// including leading/trailing days from adjacent months needed to fill
/// complete weeks (per `week_start`) — rows are weeks, each exactly 7
/// columns. Shared by the Month view (one grid) and each mini-month in the
/// Year view (twelve of them).
fn month_grid(d: NaiveDate, week_start: WeekStart) -> Vec<[NaiveDate; 7]> {
    let first = start_of_month(d);
    let next_month_first = add_months(first, 1);
    let grid_start = first - Duration::days(week_start.offset(first.weekday()));
    let end_offset = week_start.offset(next_month_first.weekday());
    let grid_end = if end_offset == 0 {
        next_month_first
    } else {
        next_month_first + Duration::days(7 - end_offset)
    };
    let mut rows = Vec::new();
    let mut day = grid_start;
    while day < grid_end {
        let mut row = [day; 7];
        for (i, slot) in row.iter_mut().enumerate() {
            *slot = day + Duration::days(i as i64);
        }
        rows.push(row);
        day += Duration::days(7);
    }
    rows
}

/// A distinct background for weekend day cells/columns, applied uniformly
/// across the Day/3-Day/Week/Month views.
const WEEKEND_BG: Color = Color::Rgb(28, 22, 30);
/// A distinct foreground for weekend day numbers/headers.
const WEEKEND_FG: Color = Color::Rgb(210, 150, 150);
/// A distinct background for an all-day event's line, so it reads as a
/// banner across the day rather than blending in with timed events —
/// applied instead of (never together with) the cell's own weekend/normal
/// background, and overridden by the selection background when selected.
const ALL_DAY_BG: Color = Color::Rgb(40, 55, 35);
/// The marker glyph for a single event, used everywhere one is drawn: as
/// the bullet on an event line in Day/3-Day/Week, and as the *entire*
/// representation of an event in the Month cells and Year mini-months,
/// where there is no room for text. Its color (see
/// [`CalApp::event_dot_color`]) is what tells one event's calendar — or an
/// unresolved sync conflict — apart from another's.
const EVENT_DOT: &str = "●";
/// Time labels in an *empty* slot of a Day/3-Day/Week time grid:
/// deliberately very dim, so the grid reads as a ruler behind the events
/// rather than as content competing with them.
const SLOT_LABEL_FG: Color = Color::Rgb(62, 62, 76);
/// Slightly brighter than [`SLOT_LABEL_FG`], for labels landing exactly on
/// the hour — keeps a readable hour rhythm in a sub-hourly grid.
const SLOT_LABEL_HOUR_FG: Color = Color::Rgb(88, 88, 106);
/// Drawn in every time-grid slot an event covers *after* the one it starts
/// in, so a long event reads as a continuous block down its column.
const SLOT_CONTINUATION: &str = "│";
/// Width of a ruler label (`09h`, `now`) — see [`CalApp::slot_label`].
const HOUR_LABEL_WIDTH: usize = 3;
/// At most this fraction of a column may be spent on cascade indentation,
/// capping how far [`pack_cascade`] will step a deeply-clashing event right
/// so its title always keeps most of the width. A share rather than a fixed
/// margin, so the staircase still reads in a narrow week column instead of
/// collapsing to a flat stack.
const CASCADE_INDENT_SHARE: u16 = 3;
/// Below this many columns an event line drops its leading time: the row's
/// position on the time axis already says when it is, and the summary is
/// what's actually in short supply. Does not apply to an event the packer
/// had to displace off its own row — see [`CellText`].
const TIGHT_TEXT_WIDTH: usize = 14;
/// At or above this many columns an event line has room for the works.
const FULL_TEXT_WIDTH: usize = 28;

/// How much of an event a cell has room to say.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CellText {
    /// Time range, summary, recurrence marker, location and sync badge.
    Full,
    /// Start time and summary.
    WithTime,
    /// Summary only — for a narrow lane, where the row's own position on
    /// the time axis already says when the event is.
    SummaryOnly,
}

impl CellText {
    /// Never [`CellText::Full`] — for the callers that ask for the terse
    /// form regardless of how much room they have (Month cells, which are
    /// laid out around a line budget, not a width).
    fn min_detail(self) -> CellText {
        match self {
            CellText::Full | CellText::WithTime => CellText::WithTime,
            CellText::SummaryOnly => CellText::SummaryOnly,
        }
    }

    /// The most detail that fits in `width`.
    fn for_width(width: u16) -> CellText {
        if (width as usize) >= FULL_TEXT_WIDTH {
            CellText::Full
        } else if (width as usize) >= TIGHT_TEXT_WIDTH {
            CellText::WithTime
        } else {
            CellText::SummaryOnly
        }
    }
}

fn local_time_of(ts_utc: i64) -> DateTime<Local> {
    DateTime::<Utc>::from_timestamp(ts_utc, 0)
        .unwrap_or_else(Utc::now)
        .with_timezone(&Local)
}

/// Truncate `s` to at most `max` displayed characters, appending an
/// ellipsis if anything was cut — used to keep event text inside a fixed
/// cell/column width without relying on line-wrapping (which would break
/// the line-count budget every grid cell is laid out around).
fn truncate_str(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let char_count = s.chars().count();
    if char_count <= max {
        return s.to_string();
    }
    if max == 1 {
        return "…".to_string();
    }
    let truncated: String = s.chars().take(max - 1).collect();
    format!("{}…", truncated)
}

/// Below this many usable rows a time grid is more cramped than useful
/// and [`plan_time_axis`] declines, leaving the caller on the plain
/// stacked-list rendering.
const MIN_GRID_ROWS: usize = 8;
/// Below this many usable columns a day column can't hold a time label
/// plus anything else, so the grid is skipped for the same reason.
const MIN_GRID_COL_WIDTH: u16 = 9;
/// At most this many terminal rows are reserved above the grid for all-day
/// banners, however many all-day events a day actually has — the time grid
/// is what the space is for.
const MAX_ALL_DAY_ROWS: usize = 3;
/// Candidate slot lengths, finest first: [`plan_time_axis`] takes the
/// first that fits the available height. Every one divides 1440 exactly,
/// which is what lets the axis be snapped to a slot boundary and still end
/// no later than midnight.
const SLOT_CHOICES: [u32; 6] = [15, 30, 60, 120, 180, 240];
const MINUTES_PER_DAY: i64 = 24 * 60;
/// The stretch of the day a time grid always keeps on screen, whatever the
/// events do. Without a floor like this, a day holding one lunchtime
/// meeting would be drawn as a two-hour close-up around it — technically
/// "the events in their relative positions", but no longer recognisable as
/// a day, and useless for comparing against a neighbouring column.
const DAY_CORE: (i64, i64) = (8 * 60, 20 * 60);

/// The vertical time ruler shared by every column of a Day/3-Day/Week
/// grid: `rows` consecutive slots of `slot_minutes`, the first starting
/// `start_minutes` after local midnight.
///
/// One axis is computed for the whole view rather than per column, which
/// is the entire point of the grid — 09:00 sits on the same terminal row
/// in every day, so events can be compared across days by eye.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TimeAxis {
    pub start_minutes: u32,
    pub slot_minutes: u32,
    pub rows: usize,
}

impl TimeAxis {
    /// Minutes-from-local-midnight at which `row` begins.
    pub fn row_start_minutes(&self, row: usize) -> u32 {
        self.start_minutes + self.slot_minutes * row as u32
    }

    /// Minutes-from-local-midnight at which the last row ends.
    pub fn end_minutes(&self) -> u32 {
        self.start_minutes + self.slot_minutes * self.rows as u32
    }

    /// The row `minute` falls in, clamped into the axis — an instant
    /// before the first slot pins to row 0 and one after the last slot to
    /// the last row, so a multi-day event that runs through the whole
    /// column still renders instead of vanishing.
    pub fn row_of(&self, minute: i64) -> usize {
        let rel = minute - self.start_minutes as i64;
        if rel <= 0 {
            return 0;
        }
        ((rel / self.slot_minutes as i64) as usize).min(self.rows.saturating_sub(1))
    }

    /// The inclusive first/last rows a `[start_min, end_min)` range
    /// occupies. The end is exclusive, so an event ending exactly on a
    /// slot boundary stops at the slot before it rather than claiming an
    /// extra empty row.
    pub fn row_span(&self, start_min: i64, end_min: i64) -> (usize, usize) {
        let first = self.row_of(start_min);
        let last = self.row_of((end_min - 1).max(start_min));
        (first, last.max(first))
    }
}

/// Lay out the time ruler for a Day/3-Day/Week grid.
///
/// `rows_available` is how many terminal rows the grid itself may use
/// (all-day banner rows already subtracted). `spans` is every *timed*
/// event's `[start, end)` in minutes-from-local-midnight across **all**
/// displayed days, so the resulting axis covers every column.
///
/// The axis always covers every span *and* [`DAY_CORE`], at the finest
/// slot length that fits; whatever height is left over is spent widening
/// the window around that. Returns `None` only when there isn't the
/// vertical room for a grid at all — the caller then falls back to the
/// plain stacked list.
pub fn plan_time_axis(rows_available: usize, spans: &[(i64, i64)]) -> Option<TimeAxis> {
    if rows_available < MIN_GRID_ROWS {
        return None;
    }
    // What has to be on screen: [`DAY_CORE`] widened to take in every
    // event, snapped out to whole hours.
    let (mut need_start, mut need_end) = DAY_CORE;
    for &(start, end) in spans {
        need_start = need_start.min(start.clamp(0, MINUTES_PER_DAY));
        need_end = need_end.max(end.clamp(0, MINUTES_PER_DAY));
    }
    need_start -= need_start.rem_euclid(60);
    need_end = ((need_end + 59) / 60 * 60).min(MINUTES_PER_DAY);

    for slot in SLOT_CHOICES {
        let slot_min = slot as i64;
        let day_rows = (MINUTES_PER_DAY / slot_min) as usize;
        let need_first = need_start.div_euclid(slot_min);
        let need_last = (need_end - 1).max(need_start).div_euclid(slot_min);
        let need_rows = (need_last - need_first + 1) as usize;
        let rows = rows_available.min(day_rows);
        if need_rows > rows {
            continue; // too fine a slot to fit what must be shown
        }
        // Spend the leftover rows widening the window around what must be
        // shown, so the grid fills its column instead of floating in it,
        // then slide the whole thing back inside the day at either end.
        let first_row =
            (need_first - ((rows - need_rows) / 2) as i64).clamp(0, (day_rows - rows) as i64);
        return Some(TimeAxis {
            start_minutes: (first_row * slot_min) as u32,
            slot_minutes: slot,
            rows,
        });
    }
    None
}

/// Where one event ended up on a day's grid.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Placement {
    /// The event's index within its day's event list — i.e. exactly what
    /// [`CalApp::event_cursor`] counts, so selection survives the packing.
    pub pos: usize,
    pub start_row: usize,
    pub end_row: usize,
    /// How many columns in from the left edge this event's line begins.
    /// Its text then runs to the *full* width of the column from there —
    /// the space is never divided up — and the columns to its left carry
    /// the continuation bars of the events it is nested under.
    pub indent: usize,
}

/// Lay a day's overlapping events out as a staircase.
///
/// `events` is `(pos, first_row, last_row)` per timed event. Every event
/// gets a line of its own — never half a line — so its title is always
/// readable at full width. Events that clash are drawn one under the other,
/// each starting one column further right than the one it overlaps, and the
/// columns it has stepped past carry those events' continuation bars. The
/// result reads as a staircase: you can see at a glance how many events
/// share a time, and read every one of their titles.
///
/// Two rules produce it:
///
/// * **One event begins per row.** A second event in the same slot slides
///   down to the next free line. Its line still prints its real start time
///   (see [`CellText`]), so simultaneous events read as consecutive rows
///   showing the same time.
/// * **An event nests one column deeper than everything it overlaps in
///   time**, capped at `max_indent` so the title never gets squeezed out.
pub fn pack_cascade(
    events: &[(usize, usize, usize)],
    rows: usize,
    max_indent: usize,
) -> Vec<Placement> {
    let mut placements = Vec::with_capacity(events.len());
    if rows == 0 || events.is_empty() {
        return placements;
    }
    let mut sorted = events.to_vec();
    sorted.sort_by_key(|&(pos, first, last)| (first, last, pos));

    let mut row_taken = vec![false; rows];
    // (first_row, last_row, indent) of what's already placed — the *real*
    // spans, so which events count as simultaneous doesn't change just
    // because one of them got pushed down a line.
    let mut placed_spans: Vec<(usize, usize, usize)> = Vec::with_capacity(sorted.len());

    for &(pos, first, last) in &sorted {
        let Some(start_row) = (first.min(rows - 1)..rows).find(|&r| !row_taken[r]) else {
            continue; // every line below is spoken for: nowhere honest to draw it
        };
        let indent = placed_spans
            .iter()
            .filter(|&&(other_first, other_last, _)| other_first <= last && first <= other_last)
            .map(|&(_, _, other_indent)| other_indent + 1)
            .max()
            .unwrap_or(0)
            .min(max_indent);
        row_taken[start_row] = true;
        placed_spans.push((first, last, indent));
        placements.push(Placement {
            pos,
            start_row,
            end_row: last.max(start_row).min(rows - 1),
            indent,
        });
    }
    placements
}

/// Minutes from local midnight of `day` to the local instant `ts_utc`.
/// Negative before that midnight and past [`MINUTES_PER_DAY`] after the
/// day ends, so a multi-day event keeps an offset the axis can clamp
/// meaningfully instead of being folded back into the day.
fn minutes_within_day(ts_utc: i64, day: NaiveDate) -> i64 {
    let local = local_time_of(ts_utc).naive_local();
    let midnight = day.and_hms_opt(0, 0, 0).unwrap();
    (local - midnight).num_minutes()
}

/// Expand every recurring row in `rows` into one row per occurrence
/// overlapping `[window_start_utc, window_end_utc)`; non-recurring rows
/// pass through unchanged (still subject to the same window, so a
/// non-recurring row that doesn't actually overlap is dropped too — the
/// DB query already guarantees that for non-recurring rows, but recurring
/// rows are fetched unconditionally and rely on this filter).
///
/// Every occurrence of the same recurring event shares its master row's
/// `id`/`uid`/every other field — only `dtstart_utc`/`dtend_utc` differ —
/// since editing or deleting any occurrence always acts on the single
/// underlying database row (see `calendar` module docs on
/// [`expand_occurrences`]). A row whose `RRULE` can't be parsed/validated
/// by the `rrule` crate, or whose raw ICS can't be re-parsed, falls back
/// to showing just its stored master occurrence rather than disappearing.
pub fn expand_recurring_events(
    rows: Vec<CalendarEventRow>,
    window_start_utc: i64,
    window_end_utc: i64,
) -> Vec<CalendarEventRow> {
    let (Some(window_start), Some(window_end)) = (
        DateTime::<Utc>::from_timestamp(window_start_utc, 0),
        DateTime::<Utc>::from_timestamp(window_end_utc, 0),
    ) else {
        return rows;
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        if row.rrule.is_none() {
            if row.dtstart_utc < window_end_utc && row.dtend_utc > window_start_utc {
                out.push(row);
            }
            continue;
        }
        let Ok(vevent) = row.to_vevent() else {
            out.push(row);
            continue;
        };
        for (start, end) in expand_occurrences(&vevent, window_start, window_end) {
            let mut occurrence = row.clone();
            occurrence.dtstart_utc = start.timestamp();
            occurrence.dtend_utc = end.timestamp();
            out.push(occurrence);
        }
    }
    out
}

#[derive(Clone, PartialEq, Eq)]
pub struct CalendarVisibility {
    pub url: String,
    pub display_name: String,
    pub visible: bool,
}

/// Everything that makes up "where you were looking": not just the view,
/// but the period on screen and the cell/event under the cursor. Stacked by
/// [`CalApp::push_history`] so `Esc` can put all of it back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ViewPosition {
    pub view: CalView,
    pub anchor: NaiveDate,
    pub focused_date: NaiveDate,
    pub event_cursor: usize,
}

/// How far back `Esc` can walk. Bounded only so a long session can't grow
/// the stack without limit; nobody navigates 32 levels deep on purpose.
const VIEW_HISTORY_MAX: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CalMode {
    Browse,
    Create,
    Edit,
    ConfirmDelete,
}

/// The event a [`CalMode::ConfirmDelete`] prompt is about, captured when
/// the prompt opens instead of being re-read from the cursor when it is
/// answered.
///
/// That matters because `jacal`'s event loop keeps reloading events while
/// the prompt is on screen (an incoming sync, or the periodic refresh), and
/// a reload re-sorts and re-filters the list — so the cursor can come to
/// rest on a *different* event between the user reading "Delete X?" and
/// pressing `y`. Capturing up front is the same thing [`DraftEvent`] does
/// with `editing_local_id` for the edit form.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PendingDelete {
    pub id: i64,
    pub summary: String,
    /// Whether the row carries an `RRULE`. Deleting it removes the whole
    /// series, not just the occurrence under the cursor (see the `calendar`
    /// module docs on why editing/deleting always acts on the master
    /// `VEVENT`), so the prompt has to say so before the user confirms.
    pub recurring: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CalField {
    Summary,
    Location,
    Description,
    StartDate,
    StartTime,
    EndDate,
    EndTime,
    AllDay,
    Rrule,
}

impl CalField {
    fn next(self) -> CalField {
        use CalField::*;
        match self {
            Summary => Location,
            Location => Description,
            Description => AllDay,
            AllDay => StartDate,
            StartDate => StartTime,
            StartTime => EndDate,
            EndDate => EndTime,
            EndTime => Rrule,
            Rrule => Summary,
        }
    }

    fn prev(self) -> CalField {
        use CalField::*;
        match self {
            Summary => Rrule,
            Location => Summary,
            Description => Location,
            AllDay => Description,
            StartDate => AllDay,
            StartTime => StartDate,
            EndDate => StartTime,
            EndTime => EndDate,
            Rrule => EndTime,
        }
    }
}

/// Editable state for the create/edit form. Dates/times are kept as
/// user-facing text (`YYYY-MM-DD` / `HH:MM`) rather than parsed types so
/// invalid intermediate input while typing doesn't need special-casing —
/// parsing (and reporting exactly what's wrong) happens once, on submit,
/// via [`DraftEvent::parse_times`].
#[derive(Clone)]
pub struct DraftEvent {
    pub summary: String,
    pub location: String,
    pub description: String,
    pub all_day: bool,
    pub start_date: String,
    pub start_time: String,
    pub end_date: String,
    pub end_time: String,
    pub rrule: String,
    pub status: EventStatus,
    pub calendar_url: String,
    pub field: CalField,
    /// `Some(id)` when editing an existing `calendar_events` row;
    /// `None` for a brand-new event.
    pub editing_local_id: Option<i64>,
}

impl DraftEvent {
    pub fn blank(calendar_url: &str, start: DateTime<Local>) -> Self {
        let end = start + Duration::hours(1);
        Self {
            summary: String::new(),
            location: String::new(),
            description: String::new(),
            all_day: false,
            start_date: start.format("%Y-%m-%d").to_string(),
            start_time: start.format("%H:%M").to_string(),
            end_date: end.format("%Y-%m-%d").to_string(),
            end_time: end.format("%H:%M").to_string(),
            rrule: String::new(),
            status: EventStatus::Confirmed,
            calendar_url: calendar_url.to_string(),
            field: CalField::Summary,
            editing_local_id: None,
        }
    }

    pub fn from_row(row: &CalendarEventRow) -> Result<Self, String> {
        let event = row
            .to_vevent()
            .map_err(|e| format!("could not read event for editing: {}", e))?;
        let start_local = event.dtstart.utc.with_timezone(&Local);
        let end_local = event.dtend.utc.with_timezone(&Local);
        Ok(Self {
            summary: event.summary,
            location: event.location,
            description: event.description,
            all_day: event.dtstart.all_day,
            start_date: start_local.format("%Y-%m-%d").to_string(),
            start_time: start_local.format("%H:%M").to_string(),
            end_date: end_local.format("%Y-%m-%d").to_string(),
            end_time: end_local.format("%H:%M").to_string(),
            rrule: event.rrule.unwrap_or_default(),
            status: event.status,
            calendar_url: row.calendar_url.clone(),
            field: CalField::Summary,
            editing_local_id: Some(row.id),
        })
    }

    pub fn next_field(&mut self) {
        self.field = self.field.next();
    }

    pub fn prev_field(&mut self) {
        self.field = self.field.prev();
    }

    /// Mutable access to the currently-focused field's text, or `None`
    /// for [`CalField::AllDay`] (a boolean, toggled via
    /// [`Self::toggle_all_day`] instead of typed into).
    fn focused_text_mut(&mut self) -> Option<&mut String> {
        match self.field {
            CalField::Summary => Some(&mut self.summary),
            CalField::Location => Some(&mut self.location),
            CalField::Description => Some(&mut self.description),
            CalField::StartDate => Some(&mut self.start_date),
            CalField::StartTime => Some(&mut self.start_time),
            CalField::EndDate => Some(&mut self.end_date),
            CalField::EndTime => Some(&mut self.end_time),
            CalField::Rrule => Some(&mut self.rrule),
            CalField::AllDay => None,
        }
    }

    /// Append a character to the focused field, or toggle `all_day` if
    /// that's the focused field.
    pub fn input_char(&mut self, c: char) {
        if self.field == CalField::AllDay {
            return;
        }
        if let Some(s) = self.focused_text_mut() {
            s.push(c);
        }
    }

    /// Remove the last character from the focused field.
    pub fn backspace(&mut self) {
        if let Some(s) = self.focused_text_mut() {
            s.pop();
        }
    }

    pub fn toggle_all_day(&mut self) {
        self.all_day = !self.all_day;
    }

    /// Cycle `status` Confirmed -> Tentative -> Cancelled -> Confirmed.
    pub fn cycle_status(&mut self) {
        self.status = match self.status {
            EventStatus::Confirmed => EventStatus::Tentative,
            EventStatus::Tentative => EventStatus::Cancelled,
            EventStatus::Cancelled => EventStatus::Confirmed,
        };
    }

    /// Parse the text start/end fields into [`EventTime`]s, or a
    /// human-readable error naming exactly what's wrong — never silently
    /// falls back to a guessed value. All-day events default a blank end
    /// date to one day after the start; timed events default a blank end
    /// date to the start date (same-day) but always require an explicit
    /// end time distinct from — and after — the start.
    pub fn parse_times(&self) -> Result<(EventTime, EventTime), String> {
        let start_date =
            NaiveDate::parse_from_str(self.start_date.trim(), "%Y-%m-%d").map_err(|_| {
                format!(
                    "invalid start date (expected YYYY-MM-DD): {}",
                    self.start_date
                )
            })?;

        if self.all_day {
            let end_date = if self.end_date.trim().is_empty() {
                start_date + Duration::days(1)
            } else {
                NaiveDate::parse_from_str(self.end_date.trim(), "%Y-%m-%d").map_err(|_| {
                    format!("invalid end date (expected YYYY-MM-DD): {}", self.end_date)
                })?
            };
            if end_date <= start_date {
                return Err("end date must be after start date".to_string());
            }
            return Ok((EventTime::all_day(start_date), EventTime::all_day(end_date)));
        }

        let start_time = NaiveTime::parse_from_str(self.start_time.trim(), "%H:%M")
            .map_err(|_| format!("invalid start time (expected HH:MM): {}", self.start_time))?;
        let end_date = if self.end_date.trim().is_empty() {
            start_date
        } else {
            NaiveDate::parse_from_str(self.end_date.trim(), "%Y-%m-%d")
                .map_err(|_| format!("invalid end date (expected YYYY-MM-DD): {}", self.end_date))?
        };
        let end_time = NaiveTime::parse_from_str(self.end_time.trim(), "%H:%M")
            .map_err(|_| format!("invalid end time (expected HH:MM): {}", self.end_time))?;

        let start_naive = start_date.and_time(start_time);
        let end_naive = end_date.and_time(end_time);
        if end_naive <= start_naive {
            return Err("end must be after start".to_string());
        }

        let start_utc = Local
            .from_local_datetime(&start_naive)
            .single()
            .ok_or_else(|| {
                "start time is ambiguous or does not exist in the local timezone (DST transition)"
                    .to_string()
            })?
            .with_timezone(&Utc);
        let end_utc = Local
            .from_local_datetime(&end_naive)
            .single()
            .ok_or_else(|| {
                "end time is ambiguous or does not exist in the local timezone (DST transition)"
                    .to_string()
            })?
            .with_timezone(&Utc);

        Ok((EventTime::utc(start_utc), EventTime::utc(end_utc)))
    }

    fn validated_rrule(&self) -> Result<Option<String>, String> {
        let trimmed = self.rrule.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        validate_rrule(trimmed).map_err(|e| e.to_string())?;
        Ok(Some(trimmed.to_string()))
    }

    /// Build the [`EventEdits`] to apply to an existing event (used for
    /// [`CalMode::Edit`]).
    pub fn to_event_edits(&self) -> Result<EventEdits, String> {
        if self.summary.trim().is_empty() {
            return Err("summary is required".to_string());
        }
        let (dtstart, dtend) = self.parse_times()?;
        let rrule = self.validated_rrule()?;
        Ok(EventEdits {
            summary: Some(self.summary.clone()),
            description: Some(self.description.clone()),
            location: Some(self.location.clone()),
            status: Some(self.status),
            dtstart: Some(dtstart),
            dtend: Some(dtend),
            rrule,
        })
    }

    /// Build a brand-new [`VEvent`] (used for [`CalMode::Create`]), with a
    /// freshly generated `UID`.
    pub fn to_new_vevent(&self) -> Result<VEvent, String> {
        if self.summary.trim().is_empty() {
            return Err("summary is required".to_string());
        }
        let (dtstart, dtend) = self.parse_times()?;
        let rrule = self.validated_rrule()?;
        Ok(VEvent {
            uid: generate_uid(),
            summary: self.summary.clone(),
            description: self.description.clone(),
            location: self.location.clone(),
            status: self.status,
            organizer: None,
            attendees: Vec::new(),
            dtstart,
            dtend,
            rrule,
            exdates: Vec::new(),
            alarms: Vec::new(),
            sequence: 0,
            dtstamp: None,
            raw: None,
        })
    }
}

/// A reasonably-unique iCalendar `UID` for a newly-created event, without
/// pulling in a UUID crate for one call site: process id + a nanosecond
/// timestamp is unique in practice for "one interactive user creating
/// events one at a time in one `jacal` process".
pub fn generate_uid() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("jacal-{}-{}@local", std::process::id(), nanos)
}

pub struct CalApp {
    pub current_account: String,
    pub view: CalView,
    pub week_start: WeekStart,
    pub anchor: NaiveDate,
    /// The day (Day/3-Day/Week/Month) or the 1st-of-month (Year, one
    /// entry per focused month) currently focused for navigation/CRUD —
    /// distinct from `anchor`, which tracks the visible *period*, not a
    /// specific cell within it. Moving focus past the edge of what's
    /// currently displayed shifts `anchor` (via [`Self::next_period`]/
    /// [`Self::prev_period`]) to bring it back on screen.
    pub focused_date: NaiveDate,
    /// Which event, among the ones on `focused_date`, is selected — reset
    /// to 0 whenever `focused_date` changes. Meaningless (but harmless)
    /// when that day has no events.
    pub event_cursor: usize,
    pub calendars: Vec<CalendarVisibility>,
    /// Every event last loaded for the visible range, unfiltered by
    /// calendar visibility — kept so toggling a calendar's visibility can
    /// re-filter instantly via [`Self::refilter`] without a DB round trip.
    pub all_events: Vec<CalendarEventRow>,
    /// `all_events` filtered to visible calendars, sorted by start time —
    /// what's actually displayed/selectable.
    pub events: Vec<CalendarEventRow>,
    /// The positions stepped *out of*, oldest first — what `Esc` walks back
    /// through (see [`Self::go_back`]).
    pub view_history: Vec<ViewPosition>,
    pub mode: CalMode,
    pub draft: Option<DraftEvent>,
    /// The event the delete prompt is about while `mode` is
    /// [`CalMode::ConfirmDelete`] — see [`PendingDelete`].
    pub pending_delete: Option<PendingDelete>,
    pub status: Option<String>,
    pub ipc_connected: bool,
}

impl CalApp {
    pub fn new(current_account: String, week_start: WeekStart) -> Self {
        let today = Local::now().date_naive();
        Self {
            current_account,
            view: CalView::Week,
            week_start,
            anchor: today,
            focused_date: today,
            event_cursor: 0,
            calendars: Vec::new(),
            all_events: Vec::new(),
            events: Vec::new(),
            view_history: Vec::new(),
            mode: CalMode::Browse,
            draft: None,
            pending_delete: None,
            status: None,
            ipc_connected: false,
        }
    }

    /// `[start, end)` of the currently visible period, in local dates.
    /// For Month view this is the exact calendar month — see
    /// [`Self::displayed_range`] for the (wider) range actually drawn,
    /// including the grid's leading/trailing days from adjacent months.
    pub fn visible_range(&self) -> (NaiveDate, NaiveDate) {
        match self.view {
            CalView::Day | CalView::Event => (self.anchor, self.anchor + Duration::days(1)),
            CalView::ThreeDay => (self.anchor, self.anchor + Duration::days(3)),
            CalView::Week => {
                let start = start_of_week(self.anchor, self.week_start);
                (start, start + Duration::days(7))
            }
            CalView::Month => {
                let start = start_of_month(self.anchor);
                (start, add_months(start, 1))
            }
            CalView::Year => {
                let start = NaiveDate::from_ymd_opt(self.anchor.year(), 1, 1).unwrap();
                let end = NaiveDate::from_ymd_opt(self.anchor.year() + 1, 1, 1).unwrap();
                (start, end)
            }
        }
    }

    /// The range of dates actually drawn on screen — same as
    /// [`Self::visible_range`] except for Month view, which also draws the
    /// grid's leading/trailing days from adjacent months (needed to fill
    /// complete weeks). Navigation uses this, not `visible_range`, to
    /// decide whether the focused cell is still on screen.
    pub fn displayed_range(&self) -> (NaiveDate, NaiveDate) {
        match self.view {
            CalView::Month => {
                let grid = month_grid(self.anchor, self.week_start);
                let first = grid.first().unwrap()[0];
                let last = *grid.last().unwrap().last().unwrap();
                (first, last + Duration::days(1))
            }
            _ => self.visible_range(),
        }
    }

    /// The Month view's grid, including leading/trailing days from
    /// adjacent months — rows are weeks, each exactly 7 columns, ordered
    /// per [`Self::week_start`].
    pub fn month_grid(&self) -> Vec<[NaiveDate; 7]> {
        month_grid(self.anchor, self.week_start)
    }

    /// The Year view's grid: twelve `(month_first_day, mini_month_grid)`
    /// pairs, January through December.
    pub fn year_grid(&self) -> Vec<(NaiveDate, Vec<[NaiveDate; 7]>)> {
        (1..=12u32)
            .map(|m| {
                let first = NaiveDate::from_ymd_opt(self.anchor.year(), m, 1).unwrap();
                (first, month_grid(first, self.week_start))
            })
            .collect()
    }

    pub fn next_period(&mut self) {
        self.anchor = match self.view {
            CalView::Day | CalView::Event => self.anchor + Duration::days(1),
            CalView::ThreeDay => self.anchor + Duration::days(3),
            CalView::Week => self.anchor + Duration::days(7),
            CalView::Month => add_months(self.anchor, 1),
            CalView::Year => NaiveDate::from_ymd_opt(self.anchor.year() + 1, 1, 1).unwrap(),
        };
    }

    pub fn prev_period(&mut self) {
        self.anchor = match self.view {
            CalView::Day | CalView::Event => self.anchor - Duration::days(1),
            CalView::ThreeDay => self.anchor - Duration::days(3),
            CalView::Week => self.anchor - Duration::days(7),
            CalView::Month => add_months(self.anchor, -1),
            CalView::Year => NaiveDate::from_ymd_opt(self.anchor.year() - 1, 1, 1).unwrap(),
        };
    }

    pub fn goto_today(&mut self) {
        let today = Local::now().date_naive();
        self.anchor = today;
        self.focused_date = today;
        self.event_cursor = 0;
    }

    /// Where the user is looking right now.
    pub fn position(&self) -> ViewPosition {
        ViewPosition {
            view: self.view,
            anchor: self.anchor,
            focused_date: self.focused_date,
            event_cursor: self.event_cursor,
        }
    }

    /// Remember the current position before a view change, so `Esc` can
    /// return to it.
    fn push_history(&mut self) {
        self.view_history.push(self.position());
        if self.view_history.len() > VIEW_HISTORY_MAX {
            self.view_history.remove(0);
        }
    }

    /// `Esc`: undo the last view change, restoring the whole position — so
    /// `Enter`ing a day from Month view and pressing `Esc` puts you back on
    /// the month you were reading, on the same cell, rather than one step
    /// wider than wherever you ended up. Returns `false` when there is
    /// nothing to go back to (quitting is `q`'s job, never `Esc`'s).
    pub fn go_back(&mut self) -> bool {
        let Some(previous) = self.view_history.pop() else {
            return false;
        };
        self.view = previous.view;
        self.anchor = previous.anchor;
        self.focused_date = previous.focused_date;
        self.event_cursor = previous.event_cursor;
        true
    }

    /// `Tab`: step one level narrower (see [`CalView::narrower`]),
    /// keeping the focused day. No-op in [`CalView::Event`] — there is
    /// nothing smaller to step into.
    pub fn narrow_view(&mut self) {
        if let Some(view) = self.view.narrower() {
            self.push_history();
            self.switch_view(view);
        }
    }

    /// `Shift+Tab`: step one level wider (see [`CalView::wider`]), keeping
    /// the focused day. No-op in [`CalView::Year`].
    pub fn widen_view(&mut self) {
        if let Some(view) = self.view.wider() {
            self.push_history();
            self.switch_view(view);
        }
    }

    /// Switch to `view`, re-anchoring the visible period on the currently
    /// focused day so the thing you were looking at stays on screen
    /// (Week -> Day lands on the focused day, not the week's first day).
    /// The selected event survives a move between two columnar views —
    /// which is what makes Day -> Event open the event under the cursor —
    /// but is reset when moving to/from a grid view, where "the event
    /// under the cursor" has no meaning.
    fn switch_view(&mut self, view: CalView) {
        if !(view.is_columnar() && self.view.is_columnar()) {
            self.event_cursor = 0;
        }
        self.view = view;
        self.anchor = self.focused_date;
    }

    /// Jump the whole visible period forward/back (as opposed to
    /// [`Self::move_focus_horizontal`]/[`Self::move_focus_vertical`],
    /// which move the focused cell by one day/week/month and only shift
    /// the period when that crosses its edge), re-anchoring focus to the
    /// new period's start.
    pub fn jump_period_forward(&mut self) {
        self.next_period();
        self.focused_date = self.anchor;
        self.event_cursor = 0;
    }

    pub fn jump_period_backward(&mut self) {
        self.prev_period();
        self.focused_date = self.anchor;
        self.event_cursor = 0;
    }

    /// Move `focused_date` to `new_date`, shifting the visible period
    /// (via repeated [`Self::prev_period`]/[`Self::next_period`]) as many
    /// times as needed to bring it back into [`Self::displayed_range`].
    /// Resets [`Self::event_cursor`] — moving to a different day/cell
    /// always starts from that cell's first event.
    fn set_focused_date(&mut self, new_date: NaiveDate) {
        self.focused_date = new_date;
        self.event_cursor = 0;
        // A single step is enough for every real movement this module
        // makes (±1 day, ±7 days, ±1 month, ±4 months, all ≤ one period's
        // span), but loop defensively rather than assume that invariant
        // holds forever; capped so a logic error can never hang the UI.
        for _ in 0..64 {
            let (start, end) = self.displayed_range();
            if new_date < start {
                self.prev_period();
            } else if new_date >= end {
                self.next_period();
            } else {
                break;
            }
        }
    }

    /// Move the focused day (Day/3-Day/Week/Month) or month (Year) left
    /// or right by one cell.
    pub fn move_focus_horizontal(&mut self, delta: i32) {
        let target = match self.view {
            CalView::Year => add_months(self.focused_date, delta),
            _ => self.focused_date + Duration::days(delta as i64),
        };
        self.set_focused_date(target);
    }

    /// In columnar views (Day/3-Day/Week), moves the selected *event*
    /// within the focused day. In grid views (Month: one row = one week;
    /// Year: one row = four months), moves the focused *cell* vertically.
    pub fn move_focus_vertical(&mut self, delta: i32) {
        if self.view.is_columnar() {
            let count = self.event_indices_for(self.focused_date).len();
            if count == 0 {
                return;
            }
            let new = self.event_cursor as i64 + delta as i64;
            self.event_cursor = new.clamp(0, count as i64 - 1) as usize;
            return;
        }
        let target = match self.view {
            CalView::Year => add_months(self.focused_date, delta * 4),
            _ => self.focused_date + Duration::days(delta as i64 * 7),
        };
        self.set_focused_date(target);
    }

    /// `Enter`: narrow focus one level, skipping the intermediate widths
    /// `Tab` would walk through — Year -> Month (of the focused month),
    /// Month/Week/3-Day -> Day (of the focused day), Day -> Event (the one
    /// under the cursor). No-op in Event view (nothing more specific to
    /// narrow to). [`Self::go_back`] returns from wherever this lands.
    pub fn zoom_in(&mut self) {
        let target = match self.view {
            CalView::Year => Some(CalView::Month),
            CalView::Month | CalView::Week | CalView::ThreeDay => Some(CalView::Day),
            CalView::Day => Some(CalView::Event),
            CalView::Event => None,
        };
        if let Some(view) = target {
            self.push_history();
            self.switch_view(view);
        }
    }

    /// A stable color for `calendar_url`, cycling through the same
    /// small fixed palette `jamail` uses for per-account list coloring —
    /// used for the small "dot" marker next to each event and consistent
    /// across every view.
    pub fn calendar_color(&self, calendar_url: &str) -> Color {
        let idx = self
            .calendars
            .iter()
            .position(|c| c.url == calendar_url)
            .unwrap_or(0);
        theme::ACCOUNT_COLORS[idx % theme::ACCOUNT_COLORS.len()]
    }

    /// Replace the calendar list, preserving each existing calendar's
    /// visibility toggle by URL (so a periodic re-discovery doesn't reset
    /// what the user hid). New calendars default to visible.
    pub fn set_calendars(&mut self, rows: Vec<CalendarRow>) {
        let previous = self.calendars.clone();
        self.calendars = rows
            .into_iter()
            .map(|r| {
                let visible = previous
                    .iter()
                    .find(|p| p.url == r.url)
                    .map(|p| p.visible)
                    .unwrap_or(true);
                CalendarVisibility {
                    url: r.url,
                    display_name: r.display_name,
                    visible,
                }
            })
            .collect();
    }

    /// Toggle a calendar's visibility and immediately re-filter the
    /// already-loaded event list — no DB round trip needed.
    pub fn toggle_calendar_visible(&mut self, url: &str) {
        if let Some(c) = self.calendars.iter_mut().find(|c| c.url == url) {
            c.visible = !c.visible;
        }
        self.refilter();
    }

    pub fn is_calendar_visible(&self, url: &str) -> bool {
        self.calendars
            .iter()
            .find(|c| c.url == url)
            .map(|c| c.visible)
            .unwrap_or(true)
    }

    /// Replace the loaded event list with `events`, then filter to visible
    /// calendars (see [`Self::refilter`]), clamping the current selection.
    pub fn set_events(&mut self, events: Vec<CalendarEventRow>) {
        self.all_events = events;
        self.refilter();
    }

    /// Same as [`Self::set_events`], but first expands every recurring row
    /// in `events` into one row per occurrence overlapping
    /// `[window_start_utc, window_end_utc)` via [`expand_recurring_events`].
    /// `bin/jacal.rs`'s `reload_events` calls this (not `set_events`
    /// directly) with the DB query's own window bounds, since
    /// `MailDb::get_calendar_events_in_range` now returns every recurring
    /// event's raw master row unconditionally — expansion is what turns
    /// that into the actual occurrences to display.
    pub fn set_events_expanding_recurrence(
        &mut self,
        events: Vec<CalendarEventRow>,
        window_start_utc: i64,
        window_end_utc: i64,
    ) {
        self.set_events(expand_recurring_events(
            events,
            window_start_utc,
            window_end_utc,
        ));
    }

    /// Recompute the displayed/selectable [`Self::events`] from
    /// [`Self::all_events`] using the current calendar visibility, sorted
    /// by start time, clamping the current selection into bounds.
    pub fn refilter(&mut self) {
        let mut events: Vec<CalendarEventRow> = self
            .all_events
            .iter()
            .filter(|e| self.is_calendar_visible(&e.calendar_url))
            .cloned()
            .collect();
        events.sort_by_key(|e| e.dtstart_utc);
        self.events = events;
        let count = self.event_indices_for(self.focused_date).len();
        if count == 0 {
            self.event_cursor = 0;
        } else if self.event_cursor >= count {
            self.event_cursor = count - 1;
        }
    }

    /// Indices into [`Self::events`] (ascending by start time) that occupy
    /// `day`: for a timed event, whose local start date is exactly `day`
    /// (or, on the very first day actually drawn on screen, started
    /// before the window — a multi-day event already in progress, or
    /// overlap from the DB's range query — rather than dropping it from
    /// the display entirely); for an all-day event, every day from its
    /// start date up to (and including) the day before its `dtend` —
    /// `dtend` is RFC 5545's *exclusive* end instant for all-day events,
    /// so a single-day all-day event's `dtend` is already the next day.
    pub fn event_indices_for(&self, day: NaiveDate) -> Vec<usize> {
        let catch_earlier = day == self.displayed_range().0;
        self.events
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                if e.all_day {
                    let start_date = local_date_of(e.dtstart_utc);
                    let end_date = local_date_of((e.dtend_utc - 1).max(e.dtstart_utc));
                    start_date <= day && day <= end_date
                } else {
                    let d = local_date_of(e.dtstart_utc);
                    if catch_earlier { d <= day } else { d == day }
                }
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// The event at [`Self::event_cursor`] within [`Self::focused_date`],
    /// or `None` if that day has no events (or the cursor is somehow out
    /// of range, which [`Self::refilter`]/[`Self::set_focused_date`]
    /// otherwise prevent).
    pub fn selected_event(&self) -> Option<&CalendarEventRow> {
        let indices = self.event_indices_for(self.focused_date);
        indices.get(self.event_cursor).map(|&i| &self.events[i])
    }

    pub fn begin_create(&mut self) {
        let default_calendar = self
            .calendars
            .iter()
            .find(|c| c.visible)
            .map(|c| c.url.clone())
            .unwrap_or_default();
        // Default to 9am local on the focused day — the day the user is
        // actually looking at, not "right now" (which could be a
        // different day entirely once you've navigated around).
        let start = self
            .focused_date
            .and_hms_opt(9, 0, 0)
            .and_then(|naive| Local.from_local_datetime(&naive).single())
            .unwrap_or_else(Local::now);
        self.draft = Some(DraftEvent::blank(&default_calendar, start));
        self.mode = CalMode::Create;
    }

    /// Enter edit mode for the currently-selected event. Returns an error
    /// (and leaves `mode`/`draft` untouched) if there's no selection or
    /// the row's stored ICS can't be re-parsed — never silently opens a
    /// blank/wrong form.
    pub fn begin_edit(&mut self) -> Result<(), String> {
        let row = self
            .selected_event()
            .ok_or_else(|| "no event selected".to_string())?;
        let draft = DraftEvent::from_row(row)?;
        self.draft = Some(draft);
        self.mode = CalMode::Edit;
        Ok(())
    }

    /// Open the delete prompt for the currently-selected event, capturing
    /// which event it is (see [`PendingDelete`]).
    pub fn begin_delete_confirm(&mut self) -> Result<(), String> {
        let ev = self
            .selected_event()
            .ok_or_else(|| "no event selected".to_string())?;
        self.pending_delete = Some(PendingDelete {
            id: ev.id,
            summary: ev.summary.clone(),
            recurring: ev.rrule.is_some(),
        });
        self.mode = CalMode::ConfirmDelete;
        Ok(())
    }

    /// Close the delete prompt, handing back the event it was about so the
    /// caller can act on exactly what the user was shown. Returns `None` if
    /// there wasn't one.
    pub fn take_pending_delete(&mut self) -> Option<PendingDelete> {
        self.mode = CalMode::Browse;
        self.pending_delete.take()
    }

    /// Close the delete prompt without deleting anything.
    pub fn cancel_delete_confirm(&mut self) {
        self.pending_delete = None;
        self.mode = CalMode::Browse;
    }

    pub fn cancel_form(&mut self) {
        self.draft = None;
        self.mode = CalMode::Browse;
    }

    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status = Some(message.into());
    }

    pub fn clear_status(&mut self) {
        self.status = None;
    }

    // -- Rendering (not unit-tested — see module docs) -------------------

    /// Draw the whole screen: header, the body for the current
    /// [`CalView`], status bar, and — while active — the create/edit form
    /// or delete-confirmation overlay. Each view has its own body
    /// renderer: day columns (on a shared [`TimeAxis`] where the terminal
    /// allows), the month grid, the year grid of mini-months, and the
    /// single-event detail.
    pub fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        frame.render_widget(Block::default().style(Style::default().bg(theme::BG)), area);
        let [header_area, main_area, status_area] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(area);

        self.render_header(frame, header_area);
        match self.view {
            CalView::Day => {
                let (start, _) = self.visible_range();
                self.render_columns(frame, main_area, &[start]);
            }
            CalView::ThreeDay => {
                let (start, _) = self.visible_range();
                let days: Vec<NaiveDate> = (0..3).map(|i| start + Duration::days(i)).collect();
                self.render_columns(frame, main_area, &days);
            }
            CalView::Week => {
                let (start, _) = self.visible_range();
                let days: Vec<NaiveDate> = (0..7).map(|i| start + Duration::days(i)).collect();
                self.render_columns(frame, main_area, &days);
            }
            CalView::Month => self.render_month_grid(frame, main_area),
            CalView::Year => self.render_year_grid(frame, main_area),
            CalView::Event => self.render_event_detail(frame, main_area),
        }
        self.render_status_bar(frame, status_area);

        match self.mode {
            CalMode::Create | CalMode::Edit => {
                if let Some(draft) = &self.draft {
                    self.render_form(frame, area, draft);
                }
            }
            CalMode::ConfirmDelete => self.render_delete_confirm(frame, area),
            CalMode::Browse => {}
        }
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let (start, end) = self.visible_range();
        let range_label = if self.view == CalView::Year {
            start.format("%Y").to_string()
        } else if end == start + Duration::days(1) {
            start.format("%A, %B %-d, %Y").to_string()
        } else {
            format!(
                "{} - {}",
                start.format("%b %-d"),
                (end - Duration::days(1)).format("%b %-d, %Y")
            )
        };
        let conn = if self.ipc_connected {
            "connected"
        } else {
            "connecting..."
        };
        let cal_summary = if self.calendars.is_empty() {
            "no calendars".to_string()
        } else {
            format!(
                "{}/{} calendars shown",
                self.calendars.iter().filter(|c| c.visible).count(),
                self.calendars.len()
            )
        };
        let lines = vec![
            Line::from(vec![
                Span::styled(
                    format!(" {} ", self.view.label()),
                    Style::default()
                        .fg(theme::MODE_INDICATOR)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(range_label, Style::default().fg(theme::FG_TEXT)),
                Span::raw("   "),
                Span::styled(
                    format!("focused: {}", self.focused_date.format("%a %b %-d")),
                    Style::default().fg(theme::FG_DIM),
                ),
            ]),
            Line::from(vec![
                Span::styled(
                    format!(" {} ", self.current_account),
                    Style::default().fg(theme::SENDER_COLOR),
                ),
                Span::styled(cal_summary, Style::default().fg(theme::FG_DIM)),
                Span::raw("  "),
                Span::styled(
                    format!("jamaild: {}", conn),
                    Style::default().fg(if self.ipc_connected {
                        theme::STATUS_SUCCESS
                    } else {
                        theme::STATUS_PENDING
                    }),
                ),
            ]),
        ];
        let block = Block::default()
            .borders(Borders::BOTTOM)
            .style(Style::default().bg(theme::BG_HEADER));
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }

    /// The color of `ev`'s [`EVENT_DOT`]: its calendar's stable color (see
    /// [`Self::calendar_color`]), except an unresolved sync conflict, which
    /// is always red whatever calendar it belongs to — that's the one thing
    /// that must not blend into the palette.
    fn event_dot_color(&self, ev: &CalendarEventRow) -> Color {
        if ev.local_status == crate::db::CAL_STATUS_CONFLICT {
            theme::STATUS_ERROR
        } else {
            self.calendar_color(&ev.calendar_url)
        }
    }

    /// One color standing in for a whole day's events, for the Year view's
    /// single-dot-per-day cells: red if *any* event that day is in conflict
    /// (see [`Self::event_dot_color`]), otherwise the first event's
    /// calendar color.
    fn day_dot_color(&self, indices: &[usize]) -> Color {
        if indices
            .iter()
            .any(|&i| self.events[i].local_status == crate::db::CAL_STATUS_CONFLICT)
        {
            return theme::STATUS_ERROR;
        }
        indices
            .first()
            .map(|&i| self.calendar_color(&self.events[i].calendar_url))
            .unwrap_or(theme::FG_DIM)
    }

    /// A one-line, color-coded summary of a cell's events: one
    /// [`EVENT_DOT`] per event in start order, colored per
    /// [`Self::event_dot_color`], with the tail collapsed into a dim `+N`
    /// when they don't all fit in `width`. This is the Month grid's
    /// at-a-glance layer, and the only thing that survives in a
    /// one-row-tall cell — where it still carries both how many events the
    /// day has and which calendars they came from.
    fn event_dot_strip(&self, indices: &[usize], width: u16, bg: Color) -> Line<'static> {
        let width = width as usize;
        if width == 0 || indices.is_empty() {
            return Line::from("");
        }
        let mut shown = indices.len().min(width);
        if shown < indices.len() {
            // Leave room for the "+N" tag before deciding how many dots fit.
            let tag_len = format!("+{}", indices.len() - shown).chars().count();
            shown = width.saturating_sub(tag_len);
        }
        let mut spans: Vec<Span<'static>> = indices
            .iter()
            .take(shown)
            .map(|&i| {
                Span::styled(
                    EVENT_DOT,
                    Style::default()
                        .fg(self.event_dot_color(&self.events[i]))
                        .bg(bg),
                )
            })
            .collect();
        if shown < indices.len() {
            spans.push(Span::styled(
                truncate_str(&format!("+{}", indices.len() - shown), width),
                Style::default().fg(theme::FG_DIM).bg(bg),
            ));
        }
        Line::from(spans)
    }

    /// Build the visible lines for one day's/cell's events: renders up to
    /// `max_lines` events via [`Self::render_event_line`] and appends a
    /// "+K more" line if not everything fit — except the
    /// currently-selected event (if any) is always kept visible even if
    /// that means showing one line beyond the nominal cap, so navigating
    /// to it never hides it. `compact` selects the terser (Month/Year) or
    /// fuller (Day/3-Day/Week) event text format.
    fn cell_event_lines(
        &self,
        day: NaiveDate,
        indices: &[usize],
        max_lines: usize,
        compact: bool,
        width: u16,
    ) -> Vec<Line<'static>> {
        if indices.is_empty() {
            return if compact {
                Vec::new()
            } else {
                vec![Line::from(Span::styled(
                    "No events",
                    Style::default().fg(theme::FG_DIM),
                ))]
            };
        }
        let is_focused_day = day == self.focused_date;
        let cell_bg = if is_weekend(day) {
            WEEKEND_BG
        } else {
            theme::BG
        };
        let mut show_count = if indices.len() > max_lines {
            max_lines.saturating_sub(1).max(1)
        } else {
            indices.len()
        };
        if is_focused_day && self.event_cursor < indices.len() {
            show_count = show_count.max(self.event_cursor + 1);
        }
        show_count = show_count.min(indices.len());

        let mut lines = Vec::with_capacity(show_count + 1);
        for (pos, &idx) in indices.iter().take(show_count).enumerate() {
            let ev = &self.events[idx];
            let selected = is_focused_day && pos == self.event_cursor;
            let text_level = if compact {
                CellText::for_width(width).min_detail()
            } else {
                CellText::Full
            };
            lines.push(self.render_event_line(ev, selected, text_level, width, cell_bg));
        }
        if indices.len() > show_count {
            lines.push(Line::from(Span::styled(
                format!("+{} more", indices.len() - show_count),
                Style::default()
                    .fg(theme::FG_DIM)
                    .add_modifier(Modifier::ITALIC),
            )));
        }
        lines
    }

    /// One event as a "[color dot] text" cell, padded to exactly `width`
    /// columns so side-by-side lanes line up (and so a selected event is
    /// highlighted across its whole cell). Text is truncated with an
    /// ellipsis rather than wrapped -- wrapping would blow out the fixed
    /// line budget every cell/column is laid out around.
    /// The dot's color is stable per calendar (see
    /// [`Self::calendar_color`]); `selected` highlights the whole line
    /// with a background, the same way a selected row does elsewhere in
    /// this crate. An all-day event instead gets [`ALL_DAY_BG`] (unless
    /// selected, which always wins) so it reads as a banner distinct from
    /// timed events; anything else explicitly uses `cell_bg` (the
    /// enclosing day cell's own weekend/normal background) rather than
    /// relying on transparency, so the dot's tiny background always
    /// matches its surroundings too.
    fn event_cell(
        &self,
        ev: &CalendarEventRow,
        selected: bool,
        text_level: CellText,
        width: u16,
        cell_bg: Color,
    ) -> Vec<Span<'static>> {
        let start_local = local_time_of(ev.dtstart_utc);
        let dot_color = self.event_dot_color(ev);
        let text = if text_level != CellText::Full {
            // An all-day event has no start time worth printing.
            if ev.all_day || text_level == CellText::SummaryOnly {
                ev.summary.clone()
            } else {
                format!("{} {}", start_local.format("%H:%M"), ev.summary)
            }
        } else {
            let time_label = if ev.all_day {
                "All day".to_string()
            } else {
                let end_local = local_time_of(ev.dtend_utc);
                format!(
                    "{}-{}",
                    start_local.format("%H:%M"),
                    end_local.format("%H:%M")
                )
            };
            let repeats = if ev.rrule.is_some() { " (repeats)" } else { "" };
            let location = if ev.location.is_empty() {
                String::new()
            } else {
                format!("  ({})", ev.location)
            };
            let pending = match ev.local_status.as_str() {
                crate::db::CAL_STATUS_PENDING_CREATE | crate::db::CAL_STATUS_PENDING_UPDATE => {
                    "  [pending]"
                }
                crate::db::CAL_STATUS_PENDING_DELETE => "  [deleting]",
                crate::db::CAL_STATUS_CONFLICT => "  [CONFLICT]",
                _ => "",
            };
            format!(
                "{} {}{}{}{}",
                time_label, ev.summary, repeats, location, pending
            )
        };
        let avail = (width as usize).saturating_sub(2);
        let text = truncate_str(&text, avail);
        let line_bg = if selected {
            theme::BG_SELECTED
        } else if ev.all_day {
            ALL_DAY_BG
        } else {
            cell_bg
        };
        let base_style = Style::default().bg(line_bg).fg(theme::SUBJECT_COLOR);
        let pad = avail.saturating_sub(text.chars().count());
        vec![
            Span::styled(EVENT_DOT, Style::default().fg(dot_color).bg(line_bg)),
            Span::styled(" ", base_style),
            Span::styled(text, base_style),
            Span::styled(" ".repeat(pad), base_style),
        ]
    }

    /// The same cell as a standalone full-width line.
    fn render_event_line(
        &self,
        ev: &CalendarEventRow,
        selected: bool,
        text_level: CellText,
        width: u16,
        cell_bg: Color,
    ) -> Line<'static> {
        Line::from(self.event_cell(ev, selected, text_level, width, cell_bg))
    }

    /// Every timed event on `days`, as `[start, end)` minute offsets from
    /// each day's own local midnight, clamped into that day — the input
    /// [`plan_time_axis`] needs to build one ruler covering every column.
    fn timed_spans_for(&self, days: &[NaiveDate]) -> Vec<(i64, i64)> {
        let mut spans = Vec::new();
        for &day in days {
            for &i in self.event_indices_for(day).iter() {
                let ev = &self.events[i];
                if ev.all_day {
                    continue;
                }
                let start = minutes_within_day(ev.dtstart_utc, day);
                let end = minutes_within_day(ev.dtend_utc, day);
                if end <= 0 || start >= MINUTES_PER_DAY {
                    continue;
                }
                spans.push((start.max(0), end.min(MINUTES_PER_DAY)));
            }
        }
        spans
    }

    /// The most all-day events any one of `days` carries, capped at
    /// [`MAX_ALL_DAY_ROWS`]. Reserved uniformly across every column so all
    /// the time grids below start on the same terminal row.
    fn all_day_rows_for(&self, days: &[NaiveDate]) -> usize {
        days.iter()
            .map(|&day| {
                self.event_indices_for(day)
                    .iter()
                    .filter(|&&i| self.events[i].all_day)
                    .count()
            })
            .max()
            .unwrap_or(0)
            .min(MAX_ALL_DAY_ROWS)
    }

    /// One day's events placed on `axis`: its all-day events (which have
    /// no position on a time ruler and get banner rows above the grid)
    /// and its timed ones as `(cursor position, index into
    /// [`Self::events`], first row, last row)`. The cursor position is the
    /// event's index within [`Self::event_indices_for`], i.e. exactly what
    /// [`Self::event_cursor`] counts, so selection survives the reshuffle
    /// into rows.
    #[allow(clippy::type_complexity)]
    fn layout_day(
        &self,
        day: NaiveDate,
        axis: &TimeAxis,
    ) -> (Vec<(usize, usize)>, Vec<(usize, usize, usize, usize)>) {
        let mut all_day = Vec::new();
        let mut timed = Vec::new();
        for (pos, &i) in self.event_indices_for(day).iter().enumerate() {
            let ev = &self.events[i];
            if ev.all_day {
                all_day.push((pos, i));
                continue;
            }
            let (first, last) = axis.row_span(
                minutes_within_day(ev.dtstart_utc, day),
                minutes_within_day(ev.dtend_utc, day),
            );
            timed.push((pos, i, first, last));
        }
        (all_day, timed)
    }

    /// One day column rendered as a time grid: up to `all_day_rows` all-day
    /// banner rows, then one row per slot of `axis`. A slot an event starts
    /// in shows that event; a slot it merely runs through shows a
    /// continuation bar in the calendar's color; an empty slot shows its own
    /// time in very dim gray, so the column reads as a ruler you can place
    /// events against — and against the neighbouring days, which share the
    /// axis.
    fn grid_column_lines(
        &self,
        day: NaiveDate,
        axis: &TimeAxis,
        all_day_rows: usize,
        width: u16,
        cell_bg: Color,
    ) -> Vec<Line<'static>> {
        let (all_day, timed) = self.layout_day(day, axis);
        let selected = if day == self.focused_date {
            Some(self.event_cursor)
        } else {
            None
        };
        let banner_text = CellText::for_width(width);
        let blank = || Line::from(Span::styled(String::new(), Style::default().bg(cell_bg)));
        let mut lines = Vec::with_capacity(all_day_rows + axis.rows);

        // -- All-day banners, padded out to the shared reserved height.
        let shown = if all_day.len() > all_day_rows {
            all_day_rows.saturating_sub(1)
        } else {
            all_day.len()
        };
        let mut visible: Vec<(usize, usize)> = all_day.iter().copied().take(shown).collect();
        // Never hide the event the cursor is on, even when it sorts past
        // the cap (same invariant `cell_event_lines` keeps).
        if let Some(sel) = selected
            && !visible.iter().any(|&(pos, _)| pos == sel)
            && let Some(&entry) = all_day.iter().find(|&&(pos, _)| pos == sel)
            && let Some(last) = visible.last_mut()
        {
            *last = entry;
        }
        for row in 0..all_day_rows {
            if let Some(&(pos, idx)) = visible.get(row) {
                lines.push(self.render_event_line(
                    &self.events[idx],
                    selected == Some(pos),
                    banner_text,
                    width,
                    cell_bg,
                ));
            } else if row == shown && all_day.len() > shown {
                lines.push(Line::from(Span::styled(
                    truncate_str(
                        &format!("+{} all-day", all_day.len() - shown),
                        width as usize,
                    ),
                    Style::default()
                        .fg(theme::FG_DIM)
                        .bg(cell_bg)
                        .add_modifier(Modifier::ITALIC),
                )));
            } else {
                lines.push(blank());
            }
        }

        // -- The time grid itself. Clashing events cascade: one per line,
        //    each stepped a column right of the ones it overlaps, whose
        //    continuation bars fill the columns it stepped past.
        let now_row = (day == Local::now().date_naive()).then(|| {
            let now = Local::now();
            axis.row_of(now.time().hour() as i64 * 60 + now.time().minute() as i64)
        });
        let spans: Vec<(usize, usize, usize)> = timed
            .iter()
            .map(|&(pos, _, first, last)| (pos, first, last))
            .collect();
        let placements = pack_cascade(&spans, axis.rows, (width / CASCADE_INDENT_SHARE) as usize);
        let of_pos = |pos: usize| timed.iter().find(|&&(p, ..)| p == pos);

        for row in 0..axis.rows {
            let owner = placements.iter().find(|p| p.start_row == row);
            // Bars of everything still running through this row, by column.
            // Anything at or past the owner's indent is left undrawn: the
            // owner's line runs the full width from there, and covering it
            // is the whole point of the cascade.
            let bar_limit = owner.map_or(width as usize, |o| o.indent);
            let bar_at = |col: usize| {
                placements
                    .iter()
                    .find(|p| p.indent == col && p.start_row < row && row <= p.end_row)
                    .and_then(|p| of_pos(p.pos).map(|&(pos, idx, ..)| (pos, idx)))
            };
            let first_busy = (0..bar_limit)
                .find(|&col| bar_at(col).is_some())
                .unwrap_or(bar_limit);
            let mut line: Vec<Span<'static>> = Vec::new();
            let mut col = 0usize;

            // The ruler only gets a say where nothing has claimed the left
            // edge — otherwise the hour would collide with a bar or a title.
            if first_busy > HOUR_LABEL_WIDTH && owner.is_none_or(|o| o.indent > 0) {
                let label = self.slot_label(axis, row, now_row == Some(row));
                if !label.0.is_empty() {
                    line.push(Span::styled(
                        label.0.clone(),
                        Style::default().fg(label.1).bg(cell_bg),
                    ));
                    col = label.0.chars().count();
                }
            }
            while col < width as usize {
                if let Some(o) = owner.filter(|o| o.indent == col) {
                    if let Some(&(_, idx, first_row, _)) = of_pos(o.pos) {
                        let is_selected = selected == Some(o.pos);
                        let cell_width = width - col as u16;
                        // An event the cascade slid off its own slot must
                        // print its real start time even in a narrow
                        // column, or a stacked clash claims the wrong time.
                        let mut text_level = CellText::for_width(cell_width);
                        if first_row != o.start_row && text_level == CellText::SummaryOnly {
                            text_level = CellText::WithTime;
                        }
                        line.extend(self.event_cell(
                            &self.events[idx],
                            is_selected,
                            text_level,
                            cell_width,
                            cell_bg,
                        ));
                    }
                    col = width as usize;
                    continue;
                }
                if let Some((pos, idx)) = bar_at(col) {
                    let bg = if selected == Some(pos) {
                        theme::BG_SELECTED
                    } else {
                        cell_bg
                    };
                    line.push(Span::styled(
                        SLOT_CONTINUATION,
                        Style::default()
                            .fg(self.event_dot_color(&self.events[idx]))
                            .bg(bg),
                    ));
                    col += 1;
                    continue;
                }
                // A run of empty columns up to the next thing on this row.
                let next = (col + 1..width as usize)
                    .find(|&c| {
                        owner.is_some_and(|o| o.indent == c)
                            || (c < bar_limit && bar_at(c).is_some())
                    })
                    .unwrap_or(width as usize);
                line.push(Span::styled(
                    " ".repeat(next - col),
                    Style::default().bg(cell_bg),
                ));
                col = next;
            }
            lines.push(Line::from(line));
        }
        lines
    }

    /// The ruler text for a row: the hour, and only the hour — `09h` on the
    /// rows that land on one, nothing on the rows in between. That is all
    /// the time information an empty slot needs, and it keeps the grid
    /// reading as a background ruler rather than a wall of timestamps. The
    /// slot the current time falls in says `now` instead, on today's column
    /// only. The caller decides whether there is room to draw it.
    fn slot_label(&self, axis: &TimeAxis, row: usize, is_now: bool) -> (String, Color) {
        let minutes = axis.row_start_minutes(row);
        if is_now {
            ("now".to_string(), theme::TIME_RELATIVE)
        } else if minutes.is_multiple_of(60) {
            (format!("{:02}h", minutes / 60), SLOT_LABEL_HOUR_FG)
        } else {
            (String::new(), SLOT_LABEL_FG)
        }
    }

    /// Render Day/3-Day/Week: one bordered column per entry in `days`,
    /// each with a weekday/date header (distinctly colored for weekends,
    /// today, and the keyboard-focused day).
    ///
    /// Given the room for it, each column's body is a time grid on one
    /// shared [`TimeAxis`] (see [`Self::grid_column_lines`]) so events sit
    /// at their real position in the day and line up across days. When the
    /// terminal is too short or the columns too narrow for that, it falls
    /// back to the plain stacked list.
    fn render_columns(&self, frame: &mut Frame, area: Rect, days: &[NaiveDate]) {
        let n = days.len().max(1) as u32;
        let constraints: Vec<Constraint> = (0..n).map(|_| Constraint::Ratio(1, n)).collect();
        let columns = Layout::horizontal(constraints).split(area);
        let today = Local::now().date_naive();

        // One axis for every column, decided from the narrowest/shortest
        // inner rect so no column has to render something that doesn't fit.
        let inner_height = area.height.saturating_sub(2) as usize;
        let inner_width = columns
            .iter()
            .map(|c| c.width.saturating_sub(2))
            .min()
            .unwrap_or(0);
        let all_day_rows = self.all_day_rows_for(days);
        let axis = if inner_width >= MIN_GRID_COL_WIDTH {
            plan_time_axis(
                inner_height.saturating_sub(all_day_rows),
                &self.timed_spans_for(days),
            )
        } else {
            None
        };

        for (col, &day) in columns.iter().zip(days.iter()) {
            let is_today = day == today;
            let is_focused = day == self.focused_date;
            let weekend = is_weekend(day);

            let title_style = if is_today {
                Style::default()
                    .fg(theme::UNREAD_MARKER)
                    .add_modifier(Modifier::BOLD)
            } else if weekend {
                Style::default().fg(WEEKEND_FG).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(theme::THREAD_INDICATOR)
                    .add_modifier(Modifier::BOLD)
            };
            let title = if is_today {
                format!(" {} (today) ", day.format("%a %-d %b"))
            } else {
                format!(" {} ", day.format("%a %-d %b"))
            };
            let border_style = if is_focused {
                Style::default().fg(theme::HELP_BORDER)
            } else {
                Style::default().fg(theme::THREAD_BRANCH)
            };
            let cell_bg = if weekend { WEEKEND_BG } else { theme::BG };
            let block = Block::bordered()
                .title(Span::styled(title, title_style))
                .border_style(border_style)
                .style(Style::default().bg(cell_bg));
            let inner = block.inner(*col);
            frame.render_widget(block, *col);

            // Grid rows are position-critical: one line per slot, never
            // wrapped, or the columns stop lining up with each other.
            if let Some(axis) = &axis {
                let lines = self.grid_column_lines(day, axis, all_day_rows, inner.width, cell_bg);
                frame.render_widget(
                    Paragraph::new(lines).style(Style::default().bg(cell_bg)),
                    inner,
                );
                continue;
            }
            let indices = self.event_indices_for(day);
            let lines =
                self.cell_event_lines(day, &indices, inner.height as usize, false, inner.width);
            frame.render_widget(
                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .style(Style::default().bg(cell_bg)),
                inner,
            );
        }
    }

    /// Render Month: the classic 7-column grid (per
    /// [`Self::week_start`]), rows = weeks, including leading/trailing
    /// days from adjacent months (dimmed) to fill complete weeks.
    fn render_month_grid(&self, frame: &mut Frame, area: Rect) {
        let [labels_area, grid_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
        self.render_weekday_labels(frame, labels_area);

        let grid = self.month_grid();
        let row_constraints: Vec<Constraint> = (0..grid.len() as u32)
            .map(|_| Constraint::Ratio(1, grid.len() as u32))
            .collect();
        let rows = Layout::vertical(row_constraints).split(grid_area);
        let today = Local::now().date_naive();

        for (row_area, week) in rows.iter().zip(grid.iter()) {
            let cols = Layout::horizontal([Constraint::Ratio(1, 7); 7]).split(*row_area);
            for (cell_area, &day) in cols.iter().zip(week.iter()) {
                self.render_month_cell(frame, *cell_area, day, today, self.anchor.month());
            }
        }
    }

    fn render_weekday_labels(&self, frame: &mut Frame, area: Rect) {
        let cols = Layout::horizontal([Constraint::Ratio(1, 7); 7]).split(area);
        for (i, col) in cols.iter().enumerate() {
            let label = self.week_start.column_labels()[i];
            let weekend = matches!(label, "Sat" | "Sun");
            let style = if weekend {
                Style::default().fg(WEEKEND_FG).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(theme::FG_DIM)
                    .add_modifier(Modifier::BOLD)
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(format!(" {}", label), style))),
                *col,
            );
        }
    }

    fn render_month_cell(
        &self,
        frame: &mut Frame,
        area: Rect,
        day: NaiveDate,
        today: NaiveDate,
        current_month: u32,
    ) {
        let is_today = day == today;
        let is_focused = day == self.focused_date;
        let weekend = is_weekend(day);
        let in_month = day.month() == current_month;

        let day_style = if is_today {
            Style::default()
                .fg(theme::BG)
                .bg(theme::UNREAD_MARKER)
                .add_modifier(Modifier::BOLD)
        } else if !in_month {
            Style::default().fg(theme::FG_DIM)
        } else if weekend {
            Style::default().fg(WEEKEND_FG).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(theme::FG_TEXT)
                .add_modifier(Modifier::BOLD)
        };
        let border_style = if is_focused {
            Style::default().fg(theme::HELP_BORDER)
        } else {
            Style::default().fg(theme::THREAD_BRANCH)
        };
        let cell_bg = if weekend { WEEKEND_BG } else { theme::BG };
        let block = Block::bordered()
            .title(Line::from(Span::styled(
                format!(" {} ", day.day()),
                day_style,
            )))
            .border_style(border_style)
            .style(Style::default().bg(cell_bg));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if !in_month {
            return; // leading/trailing overflow days: number only, no event detail
        }
        let indices = self.event_indices_for(day);
        // The dot strip goes first: it is the one line that fits even in a
        // one-row-tall cell (an 80x24 terminal gives a month cell exactly
        // that), and it is what makes a month's event load and its mix of
        // calendars readable at a glance. Whatever rows are left below it
        // get the usual compact event text.
        let mut lines = Vec::with_capacity(inner.height as usize);
        if !indices.is_empty() {
            lines.push(self.event_dot_strip(&indices, inner.width, cell_bg));
        }
        let text_rows = (inner.height as usize).saturating_sub(lines.len());
        if text_rows > 0 {
            lines.extend(self.cell_event_lines(day, &indices, text_rows, true, inner.width));
        }
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(cell_bg)),
            inner,
        );
    }

    /// Render Year: a 4x3 grid of mini-months (a "table of tables"), each
    /// a compact day-number-only grid (no event text -- cells are too
    /// small) with a bold/underlined day number marking days that have at
    /// least one event.
    fn render_year_grid(&self, frame: &mut Frame, area: Rect) {
        let month_rows = Layout::vertical([Constraint::Ratio(1, 3); 3]).split(area);
        let today = Local::now().date_naive();
        let months = self.year_grid();

        for (row_idx, row_area) in month_rows.iter().enumerate() {
            let cols = Layout::horizontal([Constraint::Ratio(1, 4); 4]).split(*row_area);
            for (col_idx, cell_area) in cols.iter().enumerate() {
                let month_idx = row_idx * 4 + col_idx;
                let Some((month_first, grid)) = months.get(month_idx) else {
                    continue;
                };
                self.render_mini_month(frame, *cell_area, *month_first, grid, today);
            }
        }
    }

    fn render_mini_month(
        &self,
        frame: &mut Frame,
        area: Rect,
        month_first: NaiveDate,
        grid: &[[NaiveDate; 7]],
        today: NaiveDate,
    ) {
        let is_focused_month = month_first.year() == self.focused_date.year()
            && month_first.month() == self.focused_date.month();
        let title_style = if is_focused_month {
            Style::default()
                .fg(theme::HELP_BORDER)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(theme::THREAD_INDICATOR)
                .add_modifier(Modifier::BOLD)
        };
        let border_style = if is_focused_month {
            Style::default().fg(theme::HELP_BORDER)
        } else {
            Style::default().fg(theme::THREAD_BRANCH)
        };
        let block = Block::bordered()
            .title(Span::styled(
                format!(" {} ", month_first.format("%B")),
                title_style,
            ))
            .border_style(border_style);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let [labels_area, days_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);
        let label_cols = Layout::horizontal([Constraint::Ratio(1, 7); 7]).split(labels_area);
        for (i, col) in label_cols.iter().enumerate() {
            let label = self.week_start.column_labels()[i];
            let short = &label[..1.min(label.len())];
            let weekend = matches!(label, "Sat" | "Sun");
            let style = if weekend {
                Style::default().fg(WEEKEND_FG)
            } else {
                Style::default().fg(theme::FG_DIM)
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(short, style)))
                    .alignment(ratatui::layout::Alignment::Center),
                *col,
            );
        }

        let row_areas = Layout::vertical(vec![Constraint::Ratio(1, grid.len() as u32); grid.len()])
            .split(days_area);
        for (row_area, week) in row_areas.iter().zip(grid.iter()) {
            let cols = Layout::horizontal([Constraint::Ratio(1, 7); 7]).split(*row_area);
            for (cell_area, &day) in cols.iter().zip(week.iter()) {
                let in_month = day.month() == month_first.month();
                let indices = if in_month {
                    self.event_indices_for(day)
                } else {
                    Vec::new()
                };
                let has_events = !indices.is_empty();
                let is_today = day == today;
                let is_focused_day = day == self.focused_date;
                let weekend = is_weekend(day);
                let mut style = if !in_month {
                    Style::default().fg(theme::THREAD_BRANCH)
                } else if is_today {
                    Style::default().fg(theme::BG).bg(theme::UNREAD_MARKER)
                } else if weekend {
                    Style::default().fg(WEEKEND_FG)
                } else {
                    Style::default().fg(theme::FG_TEXT)
                };
                if is_focused_day && !is_today {
                    style = style.bg(theme::BG_SELECTED);
                }
                // A day cell is 3 columns wide at any sane terminal size, so
                // it can carry "NN" plus one [`EVENT_DOT`] color-coded per
                // [`Self::day_dot_color`]. The dot column is reserved
                // (rendered as a space) even on event-free days, so the day
                // numbers stay aligned down the mini-month. Below 3 columns
                // there is no room at all and a bold/underlined number is
                // the fallback marker.
                let dot_fits = cell_area.width >= 3;
                if has_events && !dot_fits {
                    style = style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
                } else if has_events {
                    style = style.add_modifier(Modifier::BOLD);
                }
                let mut spans = vec![Span::styled(format!("{:>2}", day.day()), style)];
                if dot_fits {
                    spans.push(if has_events {
                        Span::styled(EVENT_DOT, Style::default().fg(self.day_dot_color(&indices)))
                    } else {
                        Span::raw(" ")
                    });
                }
                frame.render_widget(
                    Paragraph::new(Line::from(spans)).alignment(ratatui::layout::Alignment::Center),
                    *cell_area,
                );
            }
        }
    }

    /// Render [`CalView::Event`]: everything about the one event under the
    /// cursor — the narrowest range `Tab` steps into. Up/Down still walk
    /// the focused day's events (the view is columnar), so this doubles as
    /// a reading pane for stepping through a day.
    fn render_event_detail(&self, frame: &mut Frame, area: Rect) {
        let indices = self.event_indices_for(self.focused_date);
        let title = match self.selected_event() {
            Some(_) => format!(
                " {}  ({} of {}) ",
                self.focused_date.format("%A, %B %-d, %Y"),
                self.event_cursor + 1,
                indices.len()
            ),
            None => format!(" {} ", self.focused_date.format("%A, %B %-d, %Y")),
        };
        let block = Block::bordered()
            .title(Span::styled(
                title,
                Style::default()
                    .fg(theme::THREAD_INDICATOR)
                    .add_modifier(Modifier::BOLD),
            ))
            .border_style(Style::default().fg(theme::HELP_BORDER))
            .style(Style::default().bg(theme::BG));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let Some(ev) = self.selected_event() else {
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(Span::styled(
                        "No events on this day.",
                        Style::default().fg(theme::FG_DIM),
                    )),
                    Line::from(Span::styled(
                        "h/l to another day, Shift+Tab to step back out, n to create one.",
                        Style::default().fg(theme::FG_DIM),
                    )),
                ]),
                inner,
            );
            return;
        };

        let row = |label: &str, value: String| -> Line<'static> {
            Line::from(vec![
                Span::styled(
                    format!("{:<12}", label),
                    Style::default().fg(theme::DETAIL_HEADER_LABEL),
                ),
                Span::styled(value, Style::default().fg(theme::DETAIL_HEADER_VALUE)),
            ])
        };

        let when = if ev.all_day {
            let first = local_date_of(ev.dtstart_utc);
            let last = local_date_of((ev.dtend_utc - 1).max(ev.dtstart_utc));
            if first == last {
                format!("All day, {}", first.format("%a %-d %b %Y"))
            } else {
                format!(
                    "All day, {} - {}",
                    first.format("%a %-d %b"),
                    last.format("%a %-d %b %Y")
                )
            }
        } else {
            let start = local_time_of(ev.dtstart_utc);
            let end = local_time_of(ev.dtend_utc);
            if start.date_naive() == end.date_naive() {
                format!(
                    "{} {} - {}",
                    start.format("%a %-d %b %Y"),
                    start.format("%H:%M"),
                    end.format("%H:%M")
                )
            } else {
                format!(
                    "{} - {}",
                    start.format("%a %-d %b %Y %H:%M"),
                    end.format("%a %-d %b %Y %H:%M")
                )
            }
        };
        let calendar = self
            .calendars
            .iter()
            .find(|c| c.url == ev.calendar_url)
            .map(|c| c.display_name.clone())
            .unwrap_or_else(|| ev.calendar_url.clone());
        let sync = match ev.local_status.as_str() {
            crate::db::CAL_STATUS_PENDING_CREATE => "queued for upload".to_string(),
            crate::db::CAL_STATUS_PENDING_UPDATE => "edit queued for upload".to_string(),
            crate::db::CAL_STATUS_PENDING_DELETE => "delete queued".to_string(),
            crate::db::CAL_STATUS_CONFLICT => format!(
                "CONFLICT - {}",
                ev.local_error.as_deref().unwrap_or("resolve on the server")
            ),
            _ => "synced".to_string(),
        };

        let mut lines = vec![
            Line::from(vec![
                Span::styled(EVENT_DOT, Style::default().fg(self.event_dot_color(ev))),
                Span::raw(" "),
                Span::styled(
                    ev.summary.clone(),
                    Style::default()
                        .fg(theme::DETAIL_SUBJECT)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(""),
            row("When", when),
            row("Calendar", calendar),
        ];
        if !ev.location.is_empty() {
            lines.push(row("Location", ev.location.clone()));
        }
        if !ev.organizer.is_empty() {
            lines.push(row("Organizer", ev.organizer.clone()));
        }
        if let Some(rrule) = &ev.rrule {
            lines.push(row("Repeats", rrule.clone()));
        }
        lines.push(row("Status", ev.status.clone()));
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<12}", "Sync"),
                Style::default().fg(theme::DETAIL_HEADER_LABEL),
            ),
            Span::styled(
                sync,
                Style::default().fg(if ev.local_status == crate::db::CAL_STATUS_SYNCED {
                    theme::STATUS_SUCCESS
                } else if ev.local_status == crate::db::CAL_STATUS_CONFLICT {
                    theme::STATUS_ERROR
                } else {
                    theme::STATUS_PENDING
                }),
            ),
        ]));
        if !ev.description.is_empty() {
            lines.push(Line::from(""));
            for para in ev.description.lines() {
                lines.push(Line::from(Span::styled(
                    para.to_string(),
                    Style::default().fg(theme::DETAIL_BODY),
                )));
            }
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    fn render_status_bar(&self, frame: &mut Frame, area: Rect) {
        let text = if let Some(status) = &self.status {
            status.clone()
        } else {
            "hjkl move  [/] jump period  Tab/S-Tab narrow/widen  Enter zoom  Esc back  t today  n new  e edit  d del  1-9 cal  s sync  q quit"
                .to_string()
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                text,
                Style::default().fg(theme::STATUS_FG),
            )))
            .style(Style::default().bg(theme::STATUS_BG)),
            area,
        );
    }

    fn render_form(&self, frame: &mut Frame, area: Rect, draft: &DraftEvent) {
        let width = area.width.clamp(40, 70);
        let height = 16u16.min(area.height);
        let popup = centered_rect(width, height, area);
        frame.render_widget(Clear, popup);
        let title = if draft.editing_local_id.is_some() {
            " Edit event "
        } else {
            " New event "
        };
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .style(Style::default().bg(theme::HELP_BG).fg(theme::HELP_BORDER));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);

        let field_line = |label: &str, value: &str, field: CalField| -> Line<'static> {
            let active = draft.field == field;
            let label_style = if active {
                Style::default()
                    .fg(theme::COMPOSE_FIELD_ACTIVE)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::HELP_DESC)
            };
            let cursor = if active { "▌" } else { "" };
            Line::from(vec![
                Span::styled(format!("{:<13}", label), label_style),
                Span::styled(value.to_string(), Style::default().fg(theme::FG_TEXT)),
                Span::styled(cursor, Style::default().fg(theme::COMPOSE_CURSOR)),
            ])
        };

        let all_day_value = if draft.all_day { "yes" } else { "no" };
        let lines = vec![
            field_line("Summary:", &draft.summary, CalField::Summary),
            field_line("Location:", &draft.location, CalField::Location),
            field_line("Description:", &draft.description, CalField::Description),
            field_line("All day:", all_day_value, CalField::AllDay),
            field_line("Start date:", &draft.start_date, CalField::StartDate),
            field_line("Start time:", &draft.start_time, CalField::StartTime),
            field_line("End date:", &draft.end_date, CalField::EndDate),
            field_line("End time:", &draft.end_time, CalField::EndTime),
            field_line("Repeats:", &draft.rrule, CalField::Rrule),
            Line::from(""),
            Line::from(Span::styled(
                format!("Status: {} (Ctrl+X to cycle)", draft.status),
                Style::default().fg(theme::HELP_DESC),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Up/Down or Tab/Shift+Tab field  Space toggles All day  Enter save  Esc cancel",
                Style::default().fg(theme::FG_DIM),
            )),
        ];
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    fn render_delete_confirm(&self, frame: &mut Frame, area: Rect) {
        let target = self.pending_delete.as_ref();
        let recurring = target.map(|t| t.recurring).unwrap_or(false);
        let popup = centered_rect(56, if recurring { 6 } else { 5 }, area);
        frame.render_widget(Clear, popup);
        let summary = target.map(|t| t.summary.as_str()).unwrap_or("this event");
        let block = Block::default()
            .title(" Delete event? ")
            .borders(Borders::ALL)
            .style(Style::default().bg(theme::HELP_BG).fg(theme::STATUS_ERROR));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let mut lines = vec![Line::from(Span::styled(
            format!("Delete \"{}\"?", summary),
            Style::default().fg(theme::FG_TEXT),
        ))];
        if recurring {
            // Deleting acts on the master VEVENT, so this is never "just
            // this occurrence" — say so before the user commits to it.
            lines.push(Line::from(Span::styled(
                "This repeats: every occurrence will be deleted.",
                Style::default().fg(theme::STATUS_ERROR),
            )));
        }
        lines.push(Line::from(Span::styled(
            "y confirm   n/Esc cancel",
            Style::default().fg(theme::FG_DIM),
        )));
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::MailDb;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    /// A UTC timestamp at `h:00`, for building test event rows.
    fn ts(y: i32, m: u32, d: u32, h: u32) -> i64 {
        Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap().timestamp()
    }

    fn test_app() -> CalApp {
        let mut app = CalApp::new("personal".to_string(), WeekStart::Monday);
        app.anchor = date(2024, 1, 15); // a Monday
        app.focused_date = date(2024, 1, 15);
        app
    }

    // -- View navigation ------------------------------------------------

    #[test]
    fn day_view_range_is_one_day() {
        let mut app = test_app();
        app.view = CalView::Day;
        assert_eq!(app.visible_range(), (date(2024, 1, 15), date(2024, 1, 16)));
    }

    #[test]
    fn three_day_view_range_is_three_days() {
        let mut app = test_app();
        app.view = CalView::ThreeDay;
        assert_eq!(app.visible_range(), (date(2024, 1, 15), date(2024, 1, 18)));
    }

    #[test]
    fn week_view_aligns_to_monday() {
        let mut app = test_app();
        app.view = CalView::Week;
        app.anchor = date(2024, 1, 17); // a Wednesday
        assert_eq!(app.visible_range(), (date(2024, 1, 15), date(2024, 1, 22)));
    }

    #[test]
    fn month_view_spans_the_full_calendar_month() {
        let mut app = test_app();
        app.view = CalView::Month;
        app.anchor = date(2024, 2, 10);
        assert_eq!(app.visible_range(), (date(2024, 2, 1), date(2024, 3, 1)));
    }

    #[test]
    fn next_and_prev_period_move_by_the_right_span_per_view() {
        let mut app = test_app();
        for (view, expected_forward) in [
            (CalView::Day, date(2024, 1, 16)),
            (CalView::ThreeDay, date(2024, 1, 18)),
            (CalView::Week, date(2024, 1, 22)),
        ] {
            app.anchor = date(2024, 1, 15);
            app.view = view;
            app.next_period();
            assert_eq!(app.anchor, expected_forward, "{:?} forward", view);
            app.prev_period();
            assert_eq!(app.anchor, date(2024, 1, 15), "{:?} backward", view);
        }
    }

    #[test]
    fn month_navigation_crosses_year_boundary_correctly() {
        let mut app = test_app();
        app.view = CalView::Month;
        app.anchor = date(2024, 12, 15);
        app.next_period();
        assert_eq!(app.anchor, date(2025, 1, 1));
        app.prev_period();
        assert_eq!(app.anchor, date(2024, 12, 1));
        app.prev_period();
        assert_eq!(app.anchor, date(2024, 11, 1));
    }

    #[test]
    fn narrow_view_steps_strictly_from_year_down_to_a_single_event() {
        let mut app = test_app();
        app.view = CalView::Year;
        for expected in [
            CalView::Month,
            CalView::Week,
            CalView::ThreeDay,
            CalView::Day,
            CalView::Event,
        ] {
            app.narrow_view();
            assert_eq!(app.view, expected);
        }
        app.narrow_view();
        assert_eq!(
            app.view,
            CalView::Event,
            "Tab must never wrap around from Event back to Year"
        );
    }

    #[test]
    fn widen_view_is_the_exact_inverse_and_stops_at_year() {
        let mut app = test_app();
        app.view = CalView::Event;
        for expected in [
            CalView::Day,
            CalView::ThreeDay,
            CalView::Week,
            CalView::Month,
            CalView::Year,
        ] {
            app.widen_view();
            assert_eq!(app.view, expected);
        }
        app.widen_view();
        assert_eq!(app.view, CalView::Year);
    }

    #[test]
    fn narrowing_keeps_the_focused_day_and_re_anchors_the_period_on_it() {
        let mut app = test_app(); // Week view, Mon 2024-01-15
        app.focused_date = date(2024, 1, 18); // Thursday
        app.narrow_view(); // -> 3-Day
        assert_eq!(app.focused_date, date(2024, 1, 18));
        assert_eq!(app.anchor, date(2024, 1, 18));
        app.narrow_view(); // -> Day
        assert_eq!(app.visible_range(), (date(2024, 1, 18), date(2024, 1, 19)));
    }

    #[test]
    fn narrowing_from_day_to_event_keeps_the_selected_event() {
        let mut app = test_app();
        app.view = CalView::Day;
        app.set_events(vec![
            make_row("a", ts(2024, 1, 15, 9), ts(2024, 1, 15, 10), None, false),
            make_row("b", ts(2024, 1, 15, 11), ts(2024, 1, 15, 12), None, false),
        ]);
        app.event_cursor = 1;
        app.narrow_view();
        assert_eq!(app.view, CalView::Event);
        assert_eq!(app.selected_event().map(|e| e.uid.as_str()), Some("b"));
    }

    #[test]
    fn widening_out_of_a_grid_view_resets_the_event_cursor() {
        let mut app = test_app();
        app.view = CalView::Month;
        app.event_cursor = 3;
        app.widen_view();
        assert_eq!(app.view, CalView::Year);
        assert_eq!(app.event_cursor, 0);
    }

    #[test]
    fn goto_today_sets_anchor_to_current_local_date() {
        let mut app = test_app();
        app.goto_today();
        assert_eq!(app.anchor, Local::now().date_naive());
    }

    // -- Calendar selection ----------------------------------------------

    fn cal_row(url: &str, name: &str) -> CalendarRow {
        CalendarRow {
            url: url.to_string(),
            display_name: name.to_string(),
            ctag: None,
            sync_token: None,
            color: None,
        }
    }

    #[test]
    fn new_calendars_default_to_visible() {
        let mut app = test_app();
        app.set_calendars(vec![cal_row("u1", "Personal")]);
        assert!(app.is_calendar_visible("u1"));
    }

    #[test]
    fn toggling_visibility_persists_across_a_calendar_list_refresh() {
        let mut app = test_app();
        app.set_calendars(vec![cal_row("u1", "Personal"), cal_row("u2", "Work")]);
        app.toggle_calendar_visible("u1");
        assert!(!app.is_calendar_visible("u1"));

        // Re-discovery / refresh with the same URLs must not reset the toggle.
        app.set_calendars(vec![cal_row("u1", "Personal"), cal_row("u2", "Work")]);
        assert!(!app.is_calendar_visible("u1"));
        assert!(app.is_calendar_visible("u2"));
    }

    #[test]
    fn unknown_calendar_url_is_visible_by_default() {
        let app = test_app();
        assert!(app.is_calendar_visible("never-seen"));
    }

    // -- Event list selection --------------------------------------------

    fn sample_row(
        db: &MailDb,
        account: &str,
        calendar_url: &str,
        uid: &str,
        start_ts: i64,
    ) -> CalendarEventRow {
        let event = crate::calendar::parse_vevents(&format!(
            "BEGIN:VEVENT\r\nUID:{}\r\nDTSTART:20240115T090000Z\r\nDTEND:20240115T100000Z\r\nSUMMARY:{}\r\nEND:VEVENT\r\n",
            uid, uid
        ))
        .unwrap()
        .remove(0);
        let id = db
            .upsert_calendar_event(
                account,
                calendar_url,
                &format!("/{}.ics", uid),
                None,
                &event,
                event.raw.as_ref().unwrap(),
            )
            .unwrap();
        let mut row = db.get_calendar_event_by_id(id).unwrap().unwrap();
        row.dtstart_utc = start_ts; // only used for ordering in this test helper
        row
    }

    /// Build a [`CalendarEventRow`] directly (no DB round trip needed),
    /// for tests of pure display-layer logic (`event_indices_for`,
    /// `expand_recurring_events`) that only care about a handful of
    /// fields. `rrule` (when `Some`) is embedded in a synthetic `raw_ics`
    /// too, since `CalendarEventRow::to_vevent()` prefers re-parsing raw
    /// ICS over its structured columns whenever it's present — matching
    /// what a real synced row looks like.
    fn make_row(
        uid: &str,
        dtstart_utc: i64,
        dtend_utc: i64,
        rrule: Option<&str>,
        all_day: bool,
    ) -> CalendarEventRow {
        let fmt_ts = |ts: i64| {
            DateTime::<Utc>::from_timestamp(ts, 0)
                .unwrap()
                .format(if all_day { "%Y%m%d" } else { "%Y%m%dT%H%M%SZ" })
                .to_string()
        };
        let value_date = if all_day { ";VALUE=DATE" } else { "" };
        let rrule_line = rrule
            .map(|r| format!("RRULE:{}\r\n", r))
            .unwrap_or_default();
        let raw = format!(
            "BEGIN:VEVENT\r\nUID:{uid}\r\nDTSTART{value_date}:{start}\r\nDTEND{value_date}:{end}\r\n{rrule_line}SUMMARY:{uid}\r\nEND:VEVENT\r\n",
            uid = uid,
            value_date = value_date,
            start = fmt_ts(dtstart_utc),
            end = fmt_ts(dtend_utc),
            rrule_line = rrule_line,
        );
        CalendarEventRow {
            id: 1,
            account: "personal".to_string(),
            calendar_url: "cal".to_string(),
            href: format!("/{}.ics", uid),
            etag: None,
            uid: uid.to_string(),
            summary: uid.to_string(),
            description: String::new(),
            location: String::new(),
            status: "CONFIRMED".to_string(),
            organizer: String::new(),
            dtstart_utc,
            dtend_utc,
            all_day,
            tzid: None,
            rrule: rrule.map(|r| r.to_string()),
            sequence: 0,
            raw_ics: Some(raw),
            local_status: crate::db::CAL_STATUS_SYNCED.to_string(),
            local_error: None,
        }
    }

    // -- Day-by-day grouping / event indices ------------------------------

    fn all_days_with_indices(app: &CalApp) -> Vec<(NaiveDate, Vec<usize>)> {
        let (start, end) = app.visible_range();
        let mut day = start;
        let mut out = Vec::new();
        while day < end {
            out.push((day, app.event_indices_for(day)));
            day += Duration::days(1);
        }
        out
    }

    #[test]
    fn event_indices_for_covers_every_day_even_with_no_events() {
        let app = test_app(); // Week view, anchored on Monday 2024-01-15
        let days = all_days_with_indices(&app);
        assert_eq!(days.len(), 7);
        assert!(
            days.iter().all(|(_, indices)| indices.is_empty()),
            "an empty calendar must still list every day of the period"
        );
        assert_eq!(days[0].0, date(2024, 1, 15));
        assert_eq!(days[6].0, date(2024, 1, 21));
    }

    #[test]
    fn event_indices_for_matches_every_day_an_all_day_event_spans() {
        let mut app = test_app(); // Week view, anchored on Monday 2024-01-15
        // All-day event Jan 16 - Jan 18 inclusive: `dtend` is the RFC
        // 5545-exclusive instant, so it's stored as Jan 19 00:00 UTC.
        let start = EventTime::all_day(date(2024, 1, 16)).utc.timestamp();
        let end = EventTime::all_day(date(2024, 1, 19)).utc.timestamp();
        app.set_events(vec![make_row("multi-day", start, end, None, true)]);

        assert!(app.event_indices_for(date(2024, 1, 15)).is_empty());
        assert_eq!(app.event_indices_for(date(2024, 1, 16)), vec![0]);
        assert_eq!(app.event_indices_for(date(2024, 1, 17)), vec![0]);
        assert_eq!(app.event_indices_for(date(2024, 1, 18)), vec![0]);
        assert!(app.event_indices_for(date(2024, 1, 19)).is_empty());
    }

    #[test]
    fn event_indices_for_matches_single_day_all_day_event_only_that_day() {
        let mut app = test_app();
        let start = EventTime::all_day(date(2024, 1, 17)).utc.timestamp();
        let end = EventTime::all_day(date(2024, 1, 18)).utc.timestamp();
        app.set_events(vec![make_row("one-day", start, end, None, true)]);

        assert!(app.event_indices_for(date(2024, 1, 16)).is_empty());
        assert_eq!(app.event_indices_for(date(2024, 1, 17)), vec![0]);
        assert!(app.event_indices_for(date(2024, 1, 18)).is_empty());
    }

    #[test]
    fn expand_recurring_events_finds_occurrences_outside_the_masters_own_window() {
        // Weekly event that first occurred long before the displayed window.
        let master_start = Utc
            .with_ymd_and_hms(2023, 1, 3, 9, 0, 0)
            .unwrap()
            .timestamp();
        let master_end = Utc
            .with_ymd_and_hms(2023, 1, 3, 9, 30, 0)
            .unwrap()
            .timestamp();
        let row = make_row(
            "weekly-standup",
            master_start,
            master_end,
            Some("FREQ=WEEKLY;BYDAY=TU"),
            false,
        );

        // A week in January 2024 — over a year after the master occurrence.
        let window_start = Utc
            .with_ymd_and_hms(2024, 1, 15, 0, 0, 0)
            .unwrap()
            .timestamp();
        let window_end = Utc
            .with_ymd_and_hms(2024, 1, 22, 0, 0, 0)
            .unwrap()
            .timestamp();

        let expanded = expand_recurring_events(vec![row], window_start, window_end);
        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0].uid, "weekly-standup");
        let occ_start = DateTime::<Utc>::from_timestamp(expanded[0].dtstart_utc, 0).unwrap();
        assert_eq!(
            occ_start,
            Utc.with_ymd_and_hms(2024, 1, 16, 9, 0, 0).unwrap()
        );
    }

    #[test]
    fn expand_recurring_events_produces_one_row_per_occurrence_in_window() {
        let master_start = Utc
            .with_ymd_and_hms(2024, 1, 1, 9, 0, 0)
            .unwrap()
            .timestamp();
        let master_end = Utc
            .with_ymd_and_hms(2024, 1, 1, 9, 15, 0)
            .unwrap()
            .timestamp();
        let row = make_row("daily", master_start, master_end, Some("FREQ=DAILY"), false);

        let window_start = Utc
            .with_ymd_and_hms(2024, 1, 10, 0, 0, 0)
            .unwrap()
            .timestamp();
        let window_end = Utc
            .with_ymd_and_hms(2024, 1, 13, 0, 0, 0)
            .unwrap()
            .timestamp();
        let expanded = expand_recurring_events(vec![row], window_start, window_end);
        assert_eq!(expanded.len(), 3);
        assert!(expanded.iter().all(|r| r.uid == "daily" && r.id == 1));
    }

    #[test]
    fn expand_recurring_events_drops_non_recurring_rows_outside_window() {
        let row = make_row("one-off", 1_000, 2_000, None, false);
        let expanded = expand_recurring_events(vec![row], 5_000, 6_000);
        assert!(expanded.is_empty());
    }

    #[test]
    fn expand_recurring_events_keeps_non_recurring_rows_inside_window() {
        let row = make_row("one-off", 1_000, 2_000, None, false);
        let expanded = expand_recurring_events(vec![row], 0, 10_000);
        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0].dtstart_utc, 1_000);
    }

    #[test]
    fn day_view_has_exactly_one_day() {
        let mut app = test_app();
        app.view = CalView::Day;
        let days = all_days_with_indices(&app);
        assert_eq!(days.len(), 1);
        assert_eq!(days[0].0, date(2024, 1, 15));
    }

    #[test]
    fn month_view_has_one_entry_per_day_of_the_month() {
        let mut app = test_app();
        app.view = CalView::Month;
        app.anchor = date(2024, 2, 10); // February 2024 is a leap year: 29 days
        let days = all_days_with_indices(&app);
        assert_eq!(days.len(), 29);
        assert_eq!(days[0].0, date(2024, 2, 1));
        assert_eq!(days[28].0, date(2024, 2, 29));
    }

    #[test]
    fn month_grid_includes_leading_and_trailing_days_from_adjacent_months() {
        let mut app = test_app();
        app.view = CalView::Month;
        app.anchor = date(2024, 2, 10); // Feb 1 2024 is a Thursday
        let grid = app.month_grid();
        // Monday-start week containing Feb 1 2024 begins on Jan 29.
        assert_eq!(grid[0][0], date(2024, 1, 29));
        assert_eq!(grid[0][3], date(2024, 2, 1));
        let last_row = grid.last().unwrap();
        assert!(last_row.iter().any(|d| *d == date(2024, 2, 29)));
    }

    #[test]
    fn month_grid_respects_sunday_week_start() {
        let mut app = CalApp::new("personal".to_string(), WeekStart::Sunday);
        app.view = CalView::Month;
        app.anchor = date(2024, 2, 10); // Feb 1 2024 is a Thursday
        let grid = app.month_grid();
        // Sunday-start week containing Feb 1 2024 begins on Jan 28.
        assert_eq!(grid[0][0], date(2024, 1, 28));
        assert_eq!(grid[0].last().unwrap().weekday(), Weekday::Sat);
    }

    #[test]
    fn events_group_under_the_correct_day_and_preserve_indices() {
        let db = MailDb::open_in_memory().unwrap();
        let mut app = test_app();
        app.set_calendars(vec![cal_row("c1", "Cal")]);
        let ts_mon = Utc
            .with_ymd_and_hms(2024, 1, 15, 9, 0, 0)
            .unwrap()
            .timestamp();
        let ts_wed = Utc
            .with_ymd_and_hms(2024, 1, 17, 9, 0, 0)
            .unwrap()
            .timestamp();
        let a = sample_row(&db, "personal", "c1", "a", ts_mon);
        let b = sample_row(&db, "personal", "c1", "b", ts_wed);
        app.set_events(vec![a, b]);

        let days = all_days_with_indices(&app);
        assert_eq!(days.len(), 7);
        assert_eq!(days[0].0, date(2024, 1, 15));
        assert_eq!(
            days[0].1,
            vec![0],
            "Monday must claim event index 0 (\"a\")"
        );
        assert_eq!(days[2].0, date(2024, 1, 17));
        assert_eq!(
            days[2].1,
            vec![1],
            "Wednesday must claim event index 1 (\"b\")"
        );
        for (i, (day, indices)) in days.iter().enumerate() {
            if i != 0 && i != 2 {
                assert!(indices.is_empty(), "{:?} should have no events", day);
            }
        }
    }

    #[test]
    fn event_starting_before_the_window_is_claimed_by_the_first_displayed_day() {
        let db = MailDb::open_in_memory().unwrap();
        let mut app = test_app();
        app.set_calendars(vec![cal_row("c1", "Cal")]);
        // An event whose start is technically before the visible window
        // (e.g. a multi-day event already in progress, or overlap from
        // the DB range query) must still be shown, attached to the first
        // displayed day, rather than silently dropped.
        let ts_before_window = Utc
            .with_ymd_and_hms(2024, 1, 10, 9, 0, 0)
            .unwrap()
            .timestamp();
        let ev = sample_row(&db, "personal", "c1", "early", ts_before_window);
        app.set_events(vec![ev]);

        let days = all_days_with_indices(&app);
        assert_eq!(days[0].0, date(2024, 1, 15));
        assert_eq!(days[0].1, vec![0]);
    }

    #[test]
    fn set_events_filters_out_hidden_calendars() {
        let db = MailDb::open_in_memory().unwrap();
        let mut app = test_app();
        app.set_calendars(vec![
            cal_row("visible-cal", "Visible"),
            cal_row("hidden-cal", "Hidden"),
        ]);
        app.toggle_calendar_visible("hidden-cal");

        let visible_row = sample_row(&db, "personal", "visible-cal", "v1", 100);
        let hidden_row = sample_row(&db, "personal", "hidden-cal", "h1", 200);
        app.set_events(vec![visible_row, hidden_row]);

        assert_eq!(app.events.len(), 1);
        assert_eq!(app.events[0].uid, "v1");
    }

    #[test]
    fn toggling_visibility_after_loading_refilters_without_a_new_db_load() {
        let db = MailDb::open_in_memory().unwrap();
        let mut app = test_app();
        app.set_calendars(vec![cal_row("c1", "Cal1"), cal_row("c2", "Cal2")]);
        let a = sample_row(&db, "personal", "c1", "a", 100);
        let b = sample_row(&db, "personal", "c2", "b", 200);
        app.set_events(vec![a, b]);
        assert_eq!(app.events.len(), 2);

        // Toggling visibility must immediately re-filter the already
        // loaded `all_events`, with no call to set_events needed.
        app.toggle_calendar_visible("c2");
        assert_eq!(app.events.len(), 1);
        assert_eq!(app.events[0].uid, "a");

        app.toggle_calendar_visible("c2");
        assert_eq!(app.events.len(), 2);
    }

    // -- Focus navigation (days/cells and events within a day) -----------

    #[test]
    fn move_focus_horizontal_moves_by_one_day_in_columnar_views() {
        let mut app = test_app();
        app.move_focus_horizontal(1);
        assert_eq!(app.focused_date, date(2024, 1, 16));
        app.move_focus_horizontal(-1);
        assert_eq!(app.focused_date, date(2024, 1, 15));
    }

    #[test]
    fn move_focus_horizontal_past_the_edge_shifts_the_period_in_day_view() {
        let mut app = test_app();
        app.view = CalView::Day;
        app.anchor = date(2024, 1, 15);
        app.focused_date = date(2024, 1, 15);
        app.move_focus_horizontal(1);
        assert_eq!(app.focused_date, date(2024, 1, 16));
        assert_eq!(
            app.anchor,
            date(2024, 1, 16),
            "the period must follow the focus in Day view"
        );
    }

    #[test]
    fn move_focus_horizontal_past_the_week_edge_shifts_to_the_next_week() {
        let mut app = test_app();
        app.focused_date = date(2024, 1, 21); // last day of the visible week
        app.move_focus_horizontal(1);
        assert_eq!(app.focused_date, date(2024, 1, 22));
        assert_eq!(app.anchor, date(2024, 1, 22));
        assert_eq!(app.visible_range().0, date(2024, 1, 22));
    }

    #[test]
    fn move_focus_horizontal_in_year_view_moves_by_month() {
        let mut app = test_app();
        app.view = CalView::Year;
        app.anchor = date(2024, 6, 1);
        app.focused_date = date(2024, 6, 1);
        app.move_focus_horizontal(1);
        assert_eq!(app.focused_date, date(2024, 7, 1));
    }

    #[test]
    fn move_focus_horizontal_in_year_view_past_december_advances_the_year() {
        let mut app = test_app();
        app.view = CalView::Year;
        app.anchor = date(2024, 12, 1);
        app.focused_date = date(2024, 12, 1);
        app.move_focus_horizontal(1);
        assert_eq!(app.focused_date, date(2025, 1, 1));
        assert_eq!(app.anchor.year(), 2025);
    }

    #[test]
    fn move_focus_vertical_in_month_view_moves_by_one_week() {
        let mut app = test_app();
        app.view = CalView::Month;
        app.anchor = date(2024, 2, 10);
        app.focused_date = date(2024, 2, 10);
        app.move_focus_vertical(1);
        assert_eq!(app.focused_date, date(2024, 2, 17));
    }

    #[test]
    fn move_focus_vertical_in_columnar_views_moves_the_event_cursor_not_the_day() {
        let db = MailDb::open_in_memory().unwrap();
        let mut app = test_app();
        app.set_calendars(vec![cal_row("c1", "Cal")]);
        let a = sample_row(&db, "personal", "c1", "a", 1);
        let b = sample_row(&db, "personal", "c1", "b", 2);
        app.set_events(vec![a, b]); // both land on the first displayed day (1970 << window start)

        app.move_focus_vertical(1);
        assert_eq!(app.event_cursor, 1);
        assert_eq!(
            app.focused_date,
            date(2024, 1, 15),
            "vertical movement in a columnar view must not change the focused day"
        );
        assert_eq!(app.selected_event().unwrap().uid, "b");

        app.move_focus_vertical(1); // must not overshoot past the last event
        assert_eq!(app.event_cursor, 1);

        app.move_focus_vertical(-5); // must not undershoot below zero
        assert_eq!(app.event_cursor, 0);
    }

    #[test]
    fn move_focus_vertical_on_a_day_with_no_events_is_a_no_op() {
        let mut app = test_app();
        app.move_focus_vertical(1);
        assert_eq!(app.event_cursor, 0);
        assert!(app.selected_event().is_none());
    }

    #[test]
    fn event_cursor_clamps_when_the_event_list_shrinks() {
        let db = MailDb::open_in_memory().unwrap();
        let mut app = test_app();
        app.set_calendars(vec![cal_row("c1", "Cal")]);
        let a = sample_row(&db, "personal", "c1", "a", 1);
        let b = sample_row(&db, "personal", "c1", "b", 2);
        app.set_events(vec![a, b]);
        app.move_focus_vertical(1);
        assert_eq!(app.event_cursor, 1);

        let only_a = sample_row(&db, "personal", "c1", "a", 1);
        app.set_events(vec![only_a]);
        assert_eq!(app.event_cursor, 0);
        assert_eq!(app.selected_event().unwrap().uid, "a");
    }

    #[test]
    fn zoom_in_from_year_switches_to_month_view_at_the_focused_month() {
        let mut app = test_app();
        app.view = CalView::Year;
        app.focused_date = date(2024, 6, 1);
        app.zoom_in();
        assert_eq!(app.view, CalView::Month);
        assert_eq!(app.anchor, date(2024, 6, 1));
    }

    #[test]
    fn zoom_in_from_month_switches_to_day_view_at_the_focused_day() {
        let mut app = test_app();
        app.view = CalView::Month;
        app.focused_date = date(2024, 1, 20);
        app.zoom_in();
        assert_eq!(app.view, CalView::Day);
        assert_eq!(app.anchor, date(2024, 1, 20));
    }

    #[test]
    fn zoom_in_from_day_view_opens_the_event_under_the_cursor() {
        let mut app = test_app();
        app.view = CalView::Day;
        app.zoom_in();
        assert_eq!(app.view, CalView::Event);
        assert_eq!(app.anchor, app.focused_date);
    }

    #[test]
    fn esc_returns_to_the_view_you_zoomed_out_of_not_one_step_wider() {
        // Enter from Month jumps straight to Day; Esc must go back to the
        // month, not to 3-Day (which is what one step wider would give).
        let mut app = test_app();
        app.view = CalView::Month;
        app.anchor = date(2024, 1, 1);
        app.focused_date = date(2024, 1, 18);
        app.zoom_in();
        assert_eq!(app.view, CalView::Day);
        assert!(app.go_back());
        assert_eq!(app.view, CalView::Month);
        assert_eq!(app.anchor, date(2024, 1, 1));
        assert_eq!(app.focused_date, date(2024, 1, 18));
    }

    #[test]
    fn esc_walks_back_through_every_view_step_in_turn() {
        let mut app = test_app(); // Week
        app.narrow_view(); // 3-Day
        app.narrow_view(); // Day
        app.widen_view(); // 3-Day
        for expected in [CalView::Day, CalView::ThreeDay, CalView::Week] {
            assert!(app.go_back());
            assert_eq!(app.view, expected);
        }
    }

    #[test]
    fn esc_restores_the_selected_event_it_left_behind() {
        let mut app = test_app();
        app.view = CalView::Day;
        app.set_events(vec![
            make_row("a", ts(2024, 1, 15, 9), ts(2024, 1, 15, 10), None, false),
            make_row("b", ts(2024, 1, 15, 11), ts(2024, 1, 15, 12), None, false),
        ]);
        app.event_cursor = 1;
        app.widen_view(); // -> 3-Day
        app.widen_view(); // -> Week
        app.go_back();
        app.go_back();
        assert_eq!(app.view, CalView::Day);
        assert_eq!(app.event_cursor, 1);
    }

    #[test]
    fn esc_with_nothing_to_go_back_to_reports_it_rather_than_quitting() {
        let mut app = test_app();
        assert!(!app.go_back(), "quitting is q's job, never Esc's");
        assert_eq!(app.view, CalView::Week, "and the view is left alone");
    }

    #[test]
    fn the_view_history_cannot_grow_without_bound() {
        let mut app = test_app();
        for _ in 0..(VIEW_HISTORY_MAX * 2) {
            app.narrow_view();
            app.widen_view();
        }
        assert!(app.view_history.len() <= VIEW_HISTORY_MAX);
    }

    #[test]
    fn zoom_in_from_the_event_view_is_a_no_op() {
        let mut app = test_app();
        app.view = CalView::Event;
        let before = (app.view, app.anchor);
        app.zoom_in();
        assert_eq!((app.view, app.anchor), before);
    }

    // -- Time grid (Day/3-Day/Week) --------------------------------------

    #[test]
    fn a_short_terminal_falls_back_to_the_plain_list() {
        assert!(plan_time_axis(MIN_GRID_ROWS - 1, &[]).is_none());
        assert!(plan_time_axis(MIN_GRID_ROWS, &[]).is_some());
    }

    #[test]
    fn an_empty_day_gets_a_working_day_ruler_rather_than_a_blank_24_hours() {
        let axis = plan_time_axis(24, &[]).unwrap();
        assert!(axis.start_minutes <= 8 * 60);
        assert!(axis.end_minutes() >= 20 * 60);
    }

    #[test]
    fn the_axis_always_covers_every_event_on_every_displayed_day() {
        // 07:00 on one day and 22:00 on another: one shared ruler has to
        // span both, or a column would render an event outside its own grid.
        let axis = plan_time_axis(12, &[(7 * 60, 8 * 60), (21 * 60, 22 * 60)]).unwrap();
        assert!(axis.start_minutes <= 7 * 60);
        assert!(axis.end_minutes() >= 22 * 60);
    }

    #[test]
    fn the_axis_never_runs_past_midnight_at_either_end() {
        for rows in MIN_GRID_ROWS..40 {
            for spans in [
                vec![],
                vec![(0, 30)],
                vec![(23 * 60, MINUTES_PER_DAY)],
                vec![(0, 30), (23 * 60, MINUTES_PER_DAY)],
            ] {
                let axis = plan_time_axis(rows, &spans).unwrap();
                assert!(
                    axis.end_minutes() as i64 <= MINUTES_PER_DAY,
                    "rows={}",
                    rows
                );
                assert!(axis.rows <= rows, "rows={}", rows);
                for (start, end) in &spans {
                    let (first, last) = axis.row_span(*start, *end);
                    assert!(first <= last && last < axis.rows);
                }
            }
        }
    }

    #[test]
    fn a_taller_terminal_buys_resolution_not_a_closer_crop() {
        let roomy = plan_time_axis(40, &[(9 * 60, 10 * 60)]).unwrap();
        let cramped = plan_time_axis(8, &[(9 * 60, 10 * 60)]).unwrap();
        assert!(
            roomy.slot_minutes < cramped.slot_minutes,
            "{}min vs {}min",
            roomy.slot_minutes,
            cramped.slot_minutes
        );
        for axis in [roomy, cramped] {
            assert!(axis.start_minutes as i64 <= DAY_CORE.0);
            assert!(axis.end_minutes() as i64 >= DAY_CORE.1);
        }
    }

    #[test]
    fn a_day_with_one_meeting_still_shows_the_whole_working_day() {
        let axis = plan_time_axis(24, &[(12 * 60, 13 * 60)]).unwrap();
        assert!(axis.start_minutes as i64 <= DAY_CORE.0);
        assert!(axis.end_minutes() as i64 >= DAY_CORE.1);
    }

    #[test]
    fn row_span_is_inclusive_and_treats_the_end_as_exclusive() {
        let axis = TimeAxis {
            start_minutes: 8 * 60,
            slot_minutes: 60,
            rows: 12,
        };
        // 09:00-10:00 occupies exactly the 09:00 row, not 09:00 and 10:00.
        assert_eq!(axis.row_span(9 * 60, 10 * 60), (1, 1));
        assert_eq!(axis.row_span(9 * 60, 11 * 60), (1, 2));
        // A zero-length event still claims the row it starts in.
        assert_eq!(axis.row_span(9 * 60, 9 * 60), (1, 1));
    }

    #[test]
    fn an_event_outside_the_axis_pins_to_the_nearest_edge_instead_of_vanishing() {
        let axis = TimeAxis {
            start_minutes: 8 * 60,
            slot_minutes: 60,
            rows: 12,
        };
        assert_eq!(axis.row_span(-120, 9 * 60), (0, 0)); // began yesterday
        assert_eq!(axis.row_span(23 * 60, 24 * 60), (11, 11)); // runs past the end
    }

    #[test]
    fn minutes_within_day_is_measured_from_that_days_own_local_midnight() {
        let day = date(2024, 1, 15);
        let t = ts(2024, 1, 15, 9);
        assert_eq!(
            minutes_within_day(t + 3600, day) - minutes_within_day(t, day),
            60
        );
        // The same instant is a day further along when measured from the
        // previous day's midnight.
        assert_eq!(
            minutes_within_day(t, day - Duration::days(1)) - minutes_within_day(t, day),
            MINUTES_PER_DAY
        );
    }

    // -- Overlap cascade -------------------------------------------------

    /// `(pos, first_row, last_row)` triples, as `grid_column_lines` builds them.
    fn ev(pos: usize, first: usize, last: usize) -> (usize, usize, usize) {
        (pos, first, last)
    }

    #[test]
    fn a_lone_event_starts_at_the_left_edge_on_its_own_row() {
        let placed = pack_cascade(&[ev(0, 2, 4)], 12, 8);
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].indent, 0);
        assert_eq!((placed[0].start_row, placed[0].end_row), (2, 4));
    }

    #[test]
    fn events_that_never_overlap_all_start_at_the_left_edge() {
        let placed = pack_cascade(&[ev(0, 0, 1), ev(1, 3, 4), ev(2, 7, 7)], 12, 8);
        assert!(placed.iter().all(|p| p.indent == 0));
        let starts: Vec<usize> = placed.iter().map(|p| p.start_row).collect();
        assert_eq!(starts, vec![0, 3, 7], "and each keeps its own row");
    }

    #[test]
    fn simultaneous_events_cascade_one_row_down_and_one_column_right() {
        // Three events in the same slot: three lines, stepping right, each
        // with the full remaining width for its title.
        let placed = pack_cascade(&[ev(0, 4, 4), ev(1, 4, 4), ev(2, 4, 4)], 12, 8);
        assert_eq!(placed.len(), 3);
        for (n, p) in placed.iter().enumerate() {
            assert_eq!(p.start_row, 4 + n, "one event begins per row");
            assert_eq!(p.indent, n, "each steps one column right of the last");
        }
    }

    #[test]
    fn a_staggered_clash_nests_under_what_it_overlaps() {
        // 10:00-12:00, then 10:30-12:00, then 11:00-12:00.
        let placed = pack_cascade(&[ev(0, 4, 7), ev(1, 5, 7), ev(2, 6, 7)], 12, 8);
        let indents: Vec<usize> = placed.iter().map(|p| p.indent).collect();
        assert_eq!(indents, vec![0, 1, 2]);
        let starts: Vec<usize> = placed.iter().map(|p| p.start_row).collect();
        assert_eq!(
            starts,
            vec![4, 5, 6],
            "none had to move: their slots differ"
        );
    }

    #[test]
    fn the_cascade_unwinds_once_the_overlap_ends() {
        // The clash is early; a later, unrelated event is back at the edge.
        let placed = pack_cascade(&[ev(0, 0, 1), ev(1, 0, 1), ev(2, 6, 7)], 12, 8);
        assert_eq!(placed[1].indent, 1);
        assert_eq!(
            placed[2].indent, 0,
            "a fresh event is not nested under the past"
        );
    }

    #[test]
    fn every_event_gets_a_row_of_its_own() {
        let events: Vec<(usize, usize, usize)> = (0..6).map(|k| ev(k, 3, 5)).collect();
        let placed = pack_cascade(&events, 16, 8);
        assert_eq!(placed.len(), events.len());
        let mut rows: Vec<usize> = placed.iter().map(|p| p.start_row).collect();
        rows.sort();
        rows.dedup();
        assert_eq!(rows.len(), events.len(), "no two events share a line");
        assert!(
            placed.iter().all(|p| p.start_row >= 3),
            "and none starts above its slot"
        );
    }

    #[test]
    fn the_indent_is_capped_so_a_title_always_has_room() {
        let events: Vec<(usize, usize, usize)> = (0..8).map(|k| ev(k, 0, 9)).collect();
        let placed = pack_cascade(&events, 16, 2);
        assert_eq!(placed.len(), 8);
        assert!(placed.iter().all(|p| p.indent <= 2));
    }

    #[test]
    fn an_event_with_no_free_line_below_is_dropped_rather_than_drawn_wrong() {
        // Four events clashing in the last two rows of the grid: only two
        // lines exist, so two are placed and the rest left out.
        let events: Vec<(usize, usize, usize)> = (0..4).map(|k| ev(k, 2, 2)).collect();
        let placed = pack_cascade(&events, 4, 8);
        assert_eq!(placed.len(), 2);
        assert!(placed.iter().all(|p| p.start_row < 4));
    }

    // -- Delete confirmation ---------------------------------------------

    #[test]
    fn the_delete_prompt_targets_the_event_it_was_opened_for_not_the_cursor() {
        // A sync landing while the prompt is up reloads and re-sorts the
        // list; the cursor can end up on a different event. Confirming must
        // still delete the event the prompt named.
        let mut app = test_app();
        app.view = CalView::Day;
        app.set_events(vec![make_row(
            "doomed",
            ts(2024, 1, 15, 9),
            ts(2024, 1, 15, 10),
            None,
            false,
        )]);
        app.begin_delete_confirm().unwrap();
        let doomed_id = app.pending_delete.as_ref().unwrap().id;

        let mut other = make_row(
            "survivor",
            ts(2024, 1, 15, 8),
            ts(2024, 1, 15, 9),
            None,
            false,
        );
        other.id = doomed_id + 1;
        app.set_events(vec![other]); // the reload a background sync would do

        let target = app.take_pending_delete().unwrap();
        assert_eq!(target.id, doomed_id);
        assert_eq!(target.summary, "doomed");
        assert_eq!(app.mode, CalMode::Browse);
    }

    #[test]
    fn the_delete_prompt_flags_a_recurring_event_as_a_whole_series() {
        let mut app = test_app();
        app.view = CalView::Day;
        app.set_events(vec![make_row(
            "weekly",
            ts(2024, 1, 15, 9),
            ts(2024, 1, 15, 10),
            Some("FREQ=WEEKLY"),
            false,
        )]);
        app.begin_delete_confirm().unwrap();
        assert!(app.pending_delete.as_ref().unwrap().recurring);
    }

    #[test]
    fn cancelling_the_delete_prompt_drops_the_target() {
        let mut app = test_app();
        app.view = CalView::Day;
        app.set_events(vec![make_row(
            "keep-me",
            ts(2024, 1, 15, 9),
            ts(2024, 1, 15, 10),
            None,
            false,
        )]);
        app.begin_delete_confirm().unwrap();
        app.cancel_delete_confirm();
        assert_eq!(app.mode, CalMode::Browse);
        assert!(app.pending_delete.is_none());
        assert!(app.take_pending_delete().is_none());
    }

    #[test]
    fn calendar_color_is_stable_and_cycles_by_index() {
        let mut app = test_app();
        app.set_calendars(vec![cal_row("c1", "One"), cal_row("c2", "Two")]);
        let c1 = app.calendar_color("c1");
        let c2 = app.calendar_color("c2");
        assert_ne!(
            c1, c2,
            "distinct calendars should get distinct colors (until the palette wraps)"
        );
        assert_eq!(
            app.calendar_color("c1"),
            c1,
            "color must be stable across calls"
        );
        // Unknown calendar falls back to the first color rather than panicking.
        assert_eq!(app.calendar_color("unknown"), theme::ACCOUNT_COLORS[0]);
    }

    // -- Create/edit/delete form flow -------------------------------------

    #[test]
    fn begin_create_defaults_to_the_first_visible_calendar() {
        let mut app = test_app();
        app.set_calendars(vec![cal_row("c1", "Cal1"), cal_row("c2", "Cal2")]);
        app.toggle_calendar_visible("c1");
        app.begin_create();
        assert_eq!(app.mode, CalMode::Create);
        assert_eq!(app.draft.as_ref().unwrap().calendar_url, "c2");
    }

    #[test]
    fn begin_edit_without_selection_is_an_explicit_error() {
        let mut app = test_app();
        let err = app.begin_edit().unwrap_err();
        assert!(err.contains("no event selected"));
        assert_eq!(app.mode, CalMode::Browse);
    }

    #[test]
    fn begin_delete_confirm_without_selection_is_an_explicit_error() {
        let mut app = test_app();
        assert!(app.begin_delete_confirm().is_err());
        assert_eq!(app.mode, CalMode::Browse);
    }

    #[test]
    fn cancel_form_returns_to_browse_and_drops_the_draft() {
        let mut app = test_app();
        app.set_calendars(vec![cal_row("c1", "Cal1")]);
        app.begin_create();
        app.cancel_form();
        assert_eq!(app.mode, CalMode::Browse);
        assert!(app.draft.is_none());
    }

    #[test]
    fn draft_field_navigation_wraps_both_directions() {
        let mut draft = DraftEvent::blank("c1", Local::now());
        assert_eq!(draft.field, CalField::Summary);
        draft.prev_field();
        assert_eq!(draft.field, CalField::Rrule);
        draft.next_field();
        assert_eq!(draft.field, CalField::Summary);
    }

    #[test]
    fn input_char_and_backspace_edit_the_focused_field() {
        let mut draft = DraftEvent::blank("c1", Local::now());
        draft.input_char('H');
        draft.input_char('i');
        assert_eq!(draft.summary, "Hi");
        draft.backspace();
        assert_eq!(draft.summary, "H");

        draft.next_field(); // -> Location
        draft.input_char('X');
        assert_eq!(draft.location, "X");
        assert_eq!(
            draft.summary, "H",
            "editing one field must not touch another"
        );
    }

    #[test]
    fn input_char_on_all_day_field_is_a_no_op() {
        let mut draft = DraftEvent::blank("c1", Local::now());
        draft.field = CalField::AllDay;
        draft.input_char('x');
        assert!(!draft.all_day);
    }

    #[test]
    fn toggle_all_day_flips_the_flag() {
        let mut draft = DraftEvent::blank("c1", Local::now());
        assert!(!draft.all_day);
        draft.toggle_all_day();
        assert!(draft.all_day);
        draft.toggle_all_day();
        assert!(!draft.all_day);
    }

    #[test]
    fn cycle_status_goes_through_all_three_states_and_wraps() {
        let mut draft = DraftEvent::blank("c1", Local::now());
        assert_eq!(draft.status, EventStatus::Confirmed);
        draft.cycle_status();
        assert_eq!(draft.status, EventStatus::Tentative);
        draft.cycle_status();
        assert_eq!(draft.status, EventStatus::Cancelled);
        draft.cycle_status();
        assert_eq!(draft.status, EventStatus::Confirmed);
    }

    #[test]
    fn parse_times_rejects_end_before_start() {
        let mut draft =
            DraftEvent::blank("c1", Local.with_ymd_and_hms(2024, 1, 15, 9, 0, 0).unwrap());
        draft.end_time = "08:00".to_string();
        draft.end_date = draft.start_date.clone();
        let err = draft.parse_times().unwrap_err();
        assert!(err.contains("end must be after start"));
    }

    #[test]
    fn parse_times_rejects_malformed_date() {
        let mut draft = DraftEvent::blank("c1", Local::now());
        draft.start_date = "not-a-date".to_string();
        let err = draft.parse_times().unwrap_err();
        assert!(err.contains("invalid start date"));
    }

    #[test]
    fn parse_times_all_day_defaults_end_to_next_day() {
        let mut draft =
            DraftEvent::blank("c1", Local.with_ymd_and_hms(2024, 1, 15, 9, 0, 0).unwrap());
        draft.all_day = true;
        draft.end_date = String::new();
        let (start, end) = draft.parse_times().unwrap();
        assert!(start.all_day);
        assert_eq!(end.utc, start.utc + Duration::days(1));
    }

    #[test]
    fn to_new_vevent_requires_a_summary() {
        let draft = DraftEvent::blank("c1", Local::now());
        let err = draft.to_new_vevent().unwrap_err();
        assert!(err.contains("summary is required"));
    }

    #[test]
    fn to_new_vevent_succeeds_with_valid_fields_and_generates_a_unique_uid() {
        let mut draft =
            DraftEvent::blank("c1", Local.with_ymd_and_hms(2024, 1, 15, 9, 0, 0).unwrap());
        draft.summary = "Standup".to_string();
        let ev1 = draft.to_new_vevent().unwrap();
        let ev2 = draft.to_new_vevent().unwrap();
        assert_eq!(ev1.summary, "Standup");
        assert_ne!(ev1.uid, ev2.uid, "two calls must not generate the same UID");
    }

    #[test]
    fn to_new_vevent_rejects_invalid_rrule() {
        let mut draft =
            DraftEvent::blank("c1", Local.with_ymd_and_hms(2024, 1, 15, 9, 0, 0).unwrap());
        draft.summary = "Standup".to_string();
        draft.rrule = "NOT-A-VALID-RRULE".to_string();
        assert!(draft.to_new_vevent().is_err());
    }

    #[test]
    fn to_event_edits_carries_status_and_rrule_through() {
        let mut draft =
            DraftEvent::blank("c1", Local.with_ymd_and_hms(2024, 1, 15, 9, 0, 0).unwrap());
        draft.summary = "Standup".to_string();
        draft.status = EventStatus::Tentative;
        draft.rrule = "FREQ=DAILY".to_string();
        let edits = draft.to_event_edits().unwrap();
        assert_eq!(edits.status, Some(EventStatus::Tentative));
        assert_eq!(edits.rrule.as_deref(), Some("FREQ=DAILY"));
    }

    #[test]
    fn from_row_reconstructs_editable_fields_from_a_db_row() {
        let db = MailDb::open_in_memory().unwrap();
        let event = crate::calendar::parse_vevents(
            "BEGIN:VEVENT\r\nUID:edit-me\r\nDTSTART:20240115T090000Z\r\nDTEND:20240115T100000Z\r\nSUMMARY:Original\r\nLOCATION:Room B\r\nEND:VEVENT\r\n",
        )
        .unwrap()
        .remove(0);
        let id = db
            .upsert_calendar_event(
                "personal",
                "c1",
                "/edit-me.ics",
                Some("\"v1\""),
                &event,
                event.raw.as_ref().unwrap(),
            )
            .unwrap();
        let row = db.get_calendar_event_by_id(id).unwrap().unwrap();
        let draft = DraftEvent::from_row(&row).unwrap();
        assert_eq!(draft.summary, "Original");
        assert_eq!(draft.location, "Room B");
        assert_eq!(draft.editing_local_id, Some(id));
    }
}
