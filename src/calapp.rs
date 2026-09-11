//! `jacal`'s TUI state and rendering: view navigation (day/3-day/week/
//! month), calendar selection, event list selection, the create/edit/
//! delete form state, and drawing all of it — mirrors `app.rs`'s role for
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
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveTime, TimeZone, Utc, Weekday};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CalView {
    Day,
    ThreeDay,
    Week,
    Month,
    Year,
}

impl CalView {
    pub fn label(&self) -> &'static str {
        match self {
            CalView::Day => "Day",
            CalView::ThreeDay => "3-Day",
            CalView::Week => "Week",
            CalView::Month => "Month",
            CalView::Year => "Year",
        }
    }

    /// Cycle order used by the view-switch key in `jacal`.
    pub fn next(&self) -> CalView {
        match self {
            CalView::Day => CalView::ThreeDay,
            CalView::ThreeDay => CalView::Week,
            CalView::Week => CalView::Month,
            CalView::Month => CalView::Year,
            CalView::Year => CalView::Day,
        }
    }

    pub fn prev(&self) -> CalView {
        match self {
            CalView::Day => CalView::Year,
            CalView::ThreeDay => CalView::Day,
            CalView::Week => CalView::ThreeDay,
            CalView::Month => CalView::Week,
            CalView::Year => CalView::Month,
        }
    }

    /// True for the "columns of days" views (Day/3-Day/Week), where
    /// Up/Down navigates *events within the focused day* and Left/Right
    /// moves the focused day itself. False for the grid views (Month/Year),
    /// where all four arrows move between grid cells.
    fn is_columnar(&self) -> bool {
        matches!(self, CalView::Day | CalView::ThreeDay | CalView::Week)
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CalMode {
    Browse,
    Create,
    Edit,
    ConfirmDelete,
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
    pub mode: CalMode,
    pub draft: Option<DraftEvent>,
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
            mode: CalMode::Browse,
            draft: None,
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
            CalView::Day => (self.anchor, self.anchor + Duration::days(1)),
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
            CalView::Day => self.anchor + Duration::days(1),
            CalView::ThreeDay => self.anchor + Duration::days(3),
            CalView::Week => self.anchor + Duration::days(7),
            CalView::Month => add_months(self.anchor, 1),
            CalView::Year => NaiveDate::from_ymd_opt(self.anchor.year() + 1, 1, 1).unwrap(),
        };
    }

    pub fn prev_period(&mut self) {
        self.anchor = match self.view {
            CalView::Day => self.anchor - Duration::days(1),
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

    pub fn cycle_view_forward(&mut self) {
        self.view = self.view.next();
        self.focused_date = self.anchor;
        self.event_cursor = 0;
    }

    pub fn cycle_view_backward(&mut self) {
        self.view = self.view.prev();
        self.focused_date = self.anchor;
        self.event_cursor = 0;
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

    /// Narrow focus one level: Year -> Month (of the focused month), Month
    /// -> Day (of the focused day), Week/3-Day -> Day (of the focused
    /// day). No-op in Day view (nothing more specific to narrow to).
    pub fn zoom_in(&mut self) {
        match self.view {
            CalView::Year => {
                self.anchor = self.focused_date;
                self.view = CalView::Month;
                self.event_cursor = 0;
            }
            CalView::Month | CalView::Week | CalView::ThreeDay => {
                self.anchor = self.focused_date;
                self.view = CalView::Day;
                self.event_cursor = 0;
            }
            CalView::Day => {}
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

    pub fn begin_delete_confirm(&mut self) -> Result<(), String> {
        if self.selected_event().is_none() {
            return Err("no event selected".to_string());
        }
        self.mode = CalMode::ConfirmDelete;
        Ok(())
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

    /// Draw the whole screen: header, agenda list, status bar, and —
    /// while active — the create/edit form or delete-confirmation
    /// overlay. Month view is rendered as a date-grouped agenda list, the
    /// same as Day/3-Day/Week, rather than a calendar grid — a deliberate
    /// scope decision (see CLAUDE.md-style limitations noted in the crate
    /// docs/README) that keeps one rendering path for every timeframe.
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
            lines.push(self.render_event_line(ev, selected, compact, width, cell_bg));
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

    /// One event as a "[color dot] text" line, text truncated (with an
    /// ellipsis) to fit `width` rather than wrapped -- wrapping would blow
    /// out the fixed line budget every cell/column is laid out around.
    /// The dot's color is stable per calendar (see
    /// [`Self::calendar_color`]); `selected` highlights the whole line
    /// with a background, the same way a selected row does elsewhere in
    /// this crate. An all-day event instead gets [`ALL_DAY_BG`] (unless
    /// selected, which always wins) so it reads as a banner distinct from
    /// timed events; anything else explicitly uses `cell_bg` (the
    /// enclosing day cell's own weekend/normal background) rather than
    /// relying on transparency, so the dot's tiny background always
    /// matches its surroundings too.
    fn render_event_line(
        &self,
        ev: &CalendarEventRow,
        selected: bool,
        compact: bool,
        width: u16,
        cell_bg: Color,
    ) -> Line<'static> {
        let start_local = local_time_of(ev.dtstart_utc);
        let conflict = ev.local_status == crate::db::CAL_STATUS_CONFLICT;
        let dot_color = if conflict {
            theme::STATUS_ERROR
        } else {
            self.calendar_color(&ev.calendar_url)
        };
        let text = if compact {
            if ev.all_day {
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
        Line::from(vec![
            Span::styled("*", Style::default().fg(dot_color).bg(line_bg)),
            Span::styled(" ", base_style),
            Span::styled(text, base_style),
        ])
    }

    /// Render Day/3-Day/Week: one bordered column per entry in `days`,
    /// each with a weekday/date header (distinctly colored for weekends,
    /// today, and the keyboard-focused day) and its events stacked below.
    fn render_columns(&self, frame: &mut Frame, area: Rect, days: &[NaiveDate]) {
        let n = days.len().max(1) as u32;
        let constraints: Vec<Constraint> = (0..n).map(|_| Constraint::Ratio(1, n)).collect();
        let columns = Layout::horizontal(constraints).split(area);
        let today = Local::now().date_naive();

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
        let lines = self.cell_event_lines(day, &indices, inner.height as usize, true, inner.width);
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
                let has_events = in_month && !self.event_indices_for(day).is_empty();
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
                if has_events {
                    style = style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
                }
                if is_focused_day && !is_today {
                    style = style.bg(theme::BG_SELECTED);
                }
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(format!("{:>2}", day.day()), style)))
                        .alignment(ratatui::layout::Alignment::Center),
                    *cell_area,
                );
            }
        }
    }

    fn render_status_bar(&self, frame: &mut Frame, area: Rect) {
        let text = if let Some(status) = &self.status {
            status.clone()
        } else {
            "hjkl move  [/] jump period  Enter zoom  Tab view  t today  n new  e edit  d del  1-9 cal  s sync  q quit"
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
                "Tab/Shift+Tab field  Space toggles All day  Enter save  Esc cancel",
                Style::default().fg(theme::FG_DIM),
            )),
        ];
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    fn render_delete_confirm(&self, frame: &mut Frame, area: Rect) {
        let popup = centered_rect(50, 5, area);
        frame.render_widget(Clear, popup);
        let summary = self
            .selected_event()
            .map(|e| e.summary.as_str())
            .unwrap_or("this event");
        let block = Block::default()
            .title(" Delete event? ")
            .borders(Borders::ALL)
            .style(Style::default().bg(theme::HELP_BG).fg(theme::STATUS_ERROR));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let lines = vec![
            Line::from(Span::styled(
                format!("Delete \"{}\"?", summary),
                Style::default().fg(theme::FG_TEXT),
            )),
            Line::from(Span::styled(
                "y confirm   n/Esc cancel",
                Style::default().fg(theme::FG_DIM),
            )),
        ];
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
    fn cycle_view_forward_and_backward_are_inverses() {
        let mut app = test_app();
        let start = app.view;
        for _ in 0..5 {
            app.cycle_view_forward();
        }
        assert_eq!(
            app.view, start,
            "cycling forward through all 5 views must return to the start"
        );
        app.cycle_view_backward();
        app.cycle_view_forward();
        assert_eq!(app.view, start);
    }

    #[test]
    fn cycle_view_forward_visits_year_between_month_and_day() {
        let mut app = test_app();
        app.view = CalView::Month;
        app.cycle_view_forward();
        assert_eq!(app.view, CalView::Year);
        app.cycle_view_forward();
        assert_eq!(app.view, CalView::Day);
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
    fn zoom_in_from_day_view_is_a_no_op() {
        let mut app = test_app();
        app.view = CalView::Day;
        let before = (app.view, app.anchor);
        app.zoom_in();
        assert_eq!((app.view, app.anchor), before);
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
