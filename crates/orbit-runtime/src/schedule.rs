//! Schedules: workflows started by a cron expression or a fixed interval.
//!
//! A schedule is a row in `orbit_schedules` (see [`crate::store`]): its
//! specification (workflow, input, timing and policies) as JSON, and its
//! progress (the next occurrence, the last execution, counters). Any server
//! that can write fires due occurrences from its tick:
//!
//! 1. **Claim.** `UPDATE ... WHERE next_fire_ms = <the occurrence> AND <no
//!    live claim>` sets a claim with a lease. Of several servers that find
//!    the same occurrence due (shared mode), exactly one gets it.
//! 2. **Fire.** The overlap and catch-up policies decide whether to start an
//!    execution. Its id is `<schedule-id>:<occurrence, RFC 3339 UTC>`, so a
//!    start repeated after a crash is the existing idempotent start.
//! 3. **Advance.** A second compare-and-set records the outcome, moves
//!    `next_fire_ms` to the following occurrence and drops the claim.
//!
//! A server that dies between 1 and 3 leaves a claim that expires after
//! [`CLAIM_MS`]; then any server takes the occurrence again, and the
//! deterministic id makes the second start a no-op.
//!
//! Times are milliseconds since the Unix epoch. Cron expressions are
//! evaluated in the schedule's IANA time zone (UTC by default) with the
//! time-zone database bundled into the binary.

use crate::engine::{Engine, EngineError, Result};
use crate::store::{FireRecord, ScheduleRow};
use jiff::civil::{Date, DateTime};
use jiff::tz::TimeZone;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// How long a server's claim on an occurrence lasts.
pub const CLAIM_MS: u64 = 30_000;
/// After a failed start, the occurrence is retried this much later.
pub const RETRY_MS: u64 = 5_000;
/// An occurrence handled more than this late counts as missed (catch-up policy).
pub const MISSED_AFTER_MS: u64 = 60_000;
/// Due schedules are looked up at most this often (and when one changes on this server).
pub const POLL_MS: u64 = 1_000;
/// Missed occurrences counted one by one at most (a longer outage counts this many).
const MAX_MISSED: u64 = 100_000;

/// What happens when an occurrence is due while the previous execution this
/// schedule started is still running.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Overlap {
    /// Do not start; count the occurrence as skipped.
    #[default]
    Skip,
    /// Start anyway: executions of the schedule may run at the same time.
    Allow,
    /// Cancel the previous execution (its compensation runs), then start.
    CancelPrevious,
}

/// What happens to occurrences that passed while no server could fire them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Catchup {
    /// Skip them; firing resumes with the next occurrence.
    #[default]
    SkipMissed,
    /// Start one execution for the most recent missed occurrence.
    RunOnce,
}

/// A schedule's specification: the JSON of `POST /v1/schedules` (without
/// `schedule_id` and `paused`), stored in `orbit_schedules.spec`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleSpec {
    pub workflow: String,
    /// Default: the latest registered version at each fire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default)]
    pub input: Value,
    /// Five fields: minute hour day-of-month month day-of-week.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    /// A fixed interval such as `15m` or `1h30m`, aligned to `start_at`
    /// (or to the Unix epoch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<String>,
    /// IANA time zone for `cron`, e.g. `Europe/Berlin`. Default UTC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default)]
    pub overlap: Overlap,
    #[serde(default)]
    pub catchup: Catchup,
    /// RFC 3339: no occurrence before this time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_at: Option<String>,
    /// RFC 3339: no occurrence after this time; the schedule then ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_at: Option<String>,
    /// Delay each fire by a pseudo-random amount below this duration,
    /// derived from the schedule id and the occurrence (the same on every server).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jitter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

fn invalid(msg: impl Into<String>) -> EngineError {
    EngineError::new(400, "INVALID_SCHEDULE", msg)
}

/// When a schedule fires, compiled from its specification.
#[derive(Debug, Clone)]
pub struct Plan {
    timing: Timing,
    start_ms: Option<u64>,
    end_ms: Option<u64>,
    jitter_ms: u64,
}

#[derive(Debug, Clone)]
enum Timing {
    Cron(Box<Cron>, TimeZone),
    Interval { every_ms: u64 },
}

impl ScheduleSpec {
    /// Check the timing fields and compile them.
    pub fn plan(&self) -> Result<Plan> {
        let timing = match (&self.cron, &self.interval) {
            (Some(_), Some(_)) => {
                return Err(invalid("give either `cron` or `interval`, not both"))
            }
            (None, None) => return Err(invalid("a schedule needs `cron` or `interval`")),
            (Some(c), None) => {
                let cron = Cron::parse(c).map_err(|e| invalid(format!("cron `{c}`: {e}")))?;
                let tz = match &self.timezone {
                    None => TimeZone::UTC,
                    Some(name) => TimeZone::get(name)
                        .map_err(|_| invalid(format!("unknown time zone `{name}`")))?,
                };
                Timing::Cron(Box::new(cron), tz)
            }
            (None, Some(i)) => {
                if self.timezone.is_some() {
                    return Err(invalid(
                        "`timezone` applies to `cron` only; intervals are absolute",
                    ));
                }
                let every_ms =
                    parse_duration(i).map_err(|e| invalid(format!("interval `{i}`: {e}")))?;
                if every_ms < 1_000 {
                    return Err(invalid("the interval must be at least 1s"));
                }
                Timing::Interval { every_ms }
            }
        };
        let start_ms = self
            .start_at
            .as_deref()
            .map(|t| parse_time(t).map_err(|e| invalid(format!("start_at: {e}"))))
            .transpose()?;
        let end_ms = self
            .end_at
            .as_deref()
            .map(|t| parse_time(t).map_err(|e| invalid(format!("end_at: {e}"))))
            .transpose()?;
        if let (Some(s), Some(e)) = (start_ms, end_ms) {
            if e <= s {
                return Err(invalid("end_at must be after start_at"));
            }
        }
        let jitter_ms = match &self.jitter {
            None => 0,
            Some(j) => parse_duration(j).map_err(|e| invalid(format!("jitter `{j}`: {e}")))?,
        };
        if jitter_ms > 24 * 3_600_000 {
            return Err(invalid("jitter must be at most 24h"));
        }
        Ok(Plan {
            timing,
            start_ms,
            end_ms,
            jitter_ms,
        })
    }
}

impl Plan {
    /// The first occurrence strictly after `t`, if the schedule has one.
    pub fn next_after(&self, t: u64) -> Option<u64> {
        let from = match self.start_ms {
            Some(s) if s > 0 && t < s => s - 1,
            _ => t,
        };
        let next = match &self.timing {
            Timing::Interval { every_ms } => {
                let anchor = self.start_ms.unwrap_or(0);
                if from < anchor {
                    anchor
                } else {
                    anchor + ((from - anchor) / every_ms + 1) * every_ms
                }
            }
            Timing::Cron(cron, tz) => cron.next_after(from, tz)?,
        };
        match self.end_ms {
            Some(end) if next > end => None,
            _ => Some(next),
        }
    }

    /// The next `n` occurrences after `t`.
    pub fn upcoming(&self, t: u64, n: usize) -> Vec<u64> {
        let mut out = vec![];
        let mut at = t;
        while out.len() < n {
            match self.next_after(at) {
                Some(o) => {
                    out.push(o);
                    at = o;
                }
                None => break,
            }
        }
        out
    }

    /// The fire delay of an occurrence: a deterministic value below the
    /// jitter, from the schedule id and the occurrence.
    pub fn jitter_for(&self, schedule_id: &str, occurrence: u64) -> u64 {
        if self.jitter_ms == 0 {
            return 0;
        }
        let digest = Sha256::digest(format!("{schedule_id}@{occurrence}").as_bytes());
        let mut n = [0u8; 8];
        n.copy_from_slice(&digest[..8]);
        u64::from_be_bytes(n) % self.jitter_ms
    }
}

// --------------------------------------------------------------------- cron

/// A parsed five-field cron expression (minute hour day-of-month month
/// day-of-week), with the usual extensions: lists, ranges, steps, month and
/// weekday names, `7` for Sunday, and the `@hourly` ... `@yearly` macros.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    /// The day-of-month / day-of-week field starts with `*`: as in Vixie
    /// cron, a day matches both fields when either is `*`, and either field
    /// when both are restricted.
    days_star: bool,
    weekdays_star: bool,
}

const MONTHS: &[&str] = &[
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];
const WEEKDAYS: &[&str] = &["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

fn field(
    text: &str,
    min: u32,
    max: u32,
    names: &[&str],
    what: &str,
) -> std::result::Result<u64, String> {
    let value = |s: &str| -> std::result::Result<u32, String> {
        if let Some(i) = names.iter().position(|n| n.eq_ignore_ascii_case(s)) {
            return Ok(i as u32 + min);
        }
        s.parse::<u32>()
            .map_err(|_| format!("`{s}` is not a valid {what}"))
    };
    let mut mask = 0u64;
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => {
                let step: u32 = s
                    .parse()
                    .map_err(|_| format!("`{s}` is not a valid step in the {what} field"))?;
                if step == 0 {
                    return Err(format!("the step in the {what} field must be at least 1"));
                }
                (r, Some(step))
            }
            None => (part, None),
        };
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (value(a)?, value(b)?)
        } else {
            let v = value(range)?;
            // `5/15` means from 5 to the end, every 15.
            (v, if step.is_some() { max } else { v })
        };
        let hi = if what == "day of week" && hi == 7 {
            7
        } else {
            hi
        };
        let top = if what == "day of week" { 7 } else { max };
        if lo < min || hi > top || lo > hi {
            return Err(format!(
                "`{range}` is out of range for the {what} field ({min}-{max})"
            ));
        }
        let mut v = lo;
        while v <= hi {
            let bit = if what == "day of week" && v == 7 {
                0
            } else {
                v
            };
            mask |= 1 << bit;
            v += step.unwrap_or(1);
        }
    }
    Ok(mask)
}

impl Cron {
    pub fn parse(text: &str) -> std::result::Result<Cron, String> {
        let text = text.trim();
        let expanded = match text.to_ascii_lowercase().as_str() {
            "@yearly" | "@annually" => "0 0 1 1 *".to_string(),
            "@monthly" => "0 0 1 * *".to_string(),
            "@weekly" => "0 0 * * 0".to_string(),
            "@daily" | "@midnight" => "0 0 * * *".to_string(),
            "@hourly" => "0 * * * *".to_string(),
            m if m.starts_with('@') => return Err(format!("unknown macro `{text}`")),
            _ => text.to_string(),
        };
        let parts: Vec<&str> = expanded.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(format!(
                "expected 5 fields (minute hour day-of-month month day-of-week), found {}",
                parts.len()
            ));
        }
        Ok(Cron {
            minutes: field(parts[0], 0, 59, &[], "minute")?,
            hours: field(parts[1], 0, 23, &[], "hour")? as u32,
            days: field(parts[2], 1, 31, &[], "day of month")? as u32,
            months: field(parts[3], 1, 12, MONTHS, "month")? as u16,
            weekdays: field(parts[4], 0, 6, WEEKDAYS, "day of week")? as u8,
            days_star: parts[2].starts_with('*'),
            weekdays_star: parts[4].starts_with('*'),
        })
    }

    fn day_matches(&self, d: Date) -> bool {
        let dom = self.days & (1 << d.day() as u32) != 0;
        let dow = self.weekdays & (1 << d.weekday().to_sunday_zero_offset() as u8) != 0;
        if self.days_star || self.weekdays_star {
            dom && dow
        } else {
            dom || dow
        }
    }

    /// The first matching minute strictly after `t` (ms), in `tz`. A local
    /// time skipped by a daylight-saving change fires at the corresponding
    /// time after the change; a local time that occurs twice fires once, at
    /// its first occurrence.
    pub fn next_after(&self, t: u64, tz: &TimeZone) -> Option<u64> {
        let ts = Timestamp::from_millisecond(t as i64).ok()?;
        let local = ts.to_zoned(tz.clone()).datetime();
        // The first whole minute after `t`.
        let start = DateTime::from_parts(
            local.date(),
            jiff::civil::time(local.hour(), local.minute(), 0, 0),
        )
        .checked_add(jiff::Span::new().minutes(1))
        .ok()?;
        let mut day = start.date();
        // Eight years covers every valid expression (29 February included).
        for _ in 0..(8 * 366 + 2) {
            if self.months & (1 << day.month() as u16) == 0 {
                day = day
                    .first_of_month()
                    .checked_add(jiff::Span::new().months(1))
                    .ok()?;
                continue;
            }
            if self.day_matches(day) {
                for h in 0..24i8 {
                    if self.hours & (1 << h) == 0 {
                        continue;
                    }
                    if day == start.date() && h < start.hour() {
                        continue;
                    }
                    for m in 0..60i8 {
                        if self.minutes & (1 << m) == 0 {
                            continue;
                        }
                        if day == start.date() && h == start.hour() && m < start.minute() {
                            continue;
                        }
                        let dt = DateTime::from_parts(day, jiff::civil::time(h, m, 0, 0));
                        let at = tz.to_ambiguous_timestamp(dt).compatible().ok()?;
                        let ms = at.as_millisecond();
                        if ms > t as i64 {
                            return Some(ms as u64);
                        }
                    }
                }
            }
            day = day.tomorrow().ok()?;
        }
        None
    }
}

// ------------------------------------------------------------- formatting

/// `15m`, `1h30m`, `500ms`, `2d`: a sum of whole numbers with units.
pub fn parse_duration(text: &str) -> std::result::Result<u64, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("empty duration".into());
    }
    let mut total: u64 = 0;
    let mut rest = text;
    while !rest.is_empty() {
        let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 {
            return Err(format!("expected a number in `{text}`"));
        }
        let n: u64 = rest[..digits]
            .parse()
            .map_err(|_| format!("number too large in `{text}`"))?;
        rest = &rest[digits..];
        let unit_len = rest.chars().take_while(|c| c.is_ascii_alphabetic()).count();
        let unit = &rest[..unit_len];
        rest = &rest[unit_len..];
        let factor = match unit {
            "ms" => 1,
            "s" => 1_000,
            "m" => 60_000,
            "h" => 3_600_000,
            "d" => 86_400_000,
            "" => return Err(format!("`{text}` is missing a unit (ms, s, m, h or d)")),
            other => return Err(format!("unknown unit `{other}` (use ms, s, m, h or d)")),
        };
        total = n
            .checked_mul(factor)
            .and_then(|v| total.checked_add(v))
            .ok_or_else(|| format!("`{text}` is too long"))?;
    }
    Ok(total)
}

/// Parse an RFC 3339 timestamp to milliseconds since the Unix epoch.
pub fn parse_time(text: &str) -> std::result::Result<u64, String> {
    let ts: Timestamp = text
        .parse()
        .map_err(|e| format!("`{text}` is not an RFC 3339 timestamp: {e}"))?;
    u64::try_from(ts.as_millisecond()).map_err(|_| format!("`{text}` is before 1970"))
}

/// Milliseconds since the Unix epoch as RFC 3339 in UTC.
pub fn format_time(ms: u64) -> String {
    Timestamp::from_millisecond(ms as i64)
        .map(|t| t.to_string())
        .unwrap_or_default()
}

/// The execution id of an occurrence: `<schedule-id>:<occurrence>`.
pub fn fire_execution_id(schedule_id: &str, occurrence: u64) -> String {
    format!("{schedule_id}:{}", format_time(occurrence))
}

/// Schedule ids: letters, digits, `-`, `_` and `.`, at most 100 characters.
pub fn valid_schedule_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 100
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

/// The API view of a schedule: its specification, its state and its next
/// fire times.
pub fn view(row: &ScheduleRow) -> Value {
    let spec: Value = serde_json::from_str(&row.spec).unwrap_or(Value::Null);
    let plan = serde_json::from_value::<ScheduleSpec>(spec.clone())
        .ok()
        .and_then(|s| s.plan().ok());
    let next = if row.paused {
        vec![]
    } else {
        match (row.next_fire_ms, &plan) {
            (Some(n), Some(p)) => {
                let mut v = vec![n];
                v.extend(p.upcoming(n, 4));
                v
            }
            _ => vec![],
        }
    };
    let mut out = json!({ "schedule_id": row.schedule_id });
    if let (Some(o), Value::Object(s)) = (out.as_object_mut(), spec) {
        o.extend(s);
    }
    let opt_time = |v: Option<u64>| v.map(format_time);
    let o = out.as_object_mut().expect("object");
    o.insert("paused".into(), json!(row.paused));
    o.insert(
        "next_fire_at".into(),
        json!(if row.paused {
            None
        } else {
            row.due_ms.map(format_time)
        }),
    );
    o.insert(
        "next_fire_times".into(),
        json!(next.iter().map(|t| format_time(*t)).collect::<Vec<_>>()),
    );
    o.insert("last_fire_at".into(), json!(opt_time(row.last_fire_ms)));
    o.insert("last_execution_id".into(), json!(row.last_execution_id));
    o.insert("fired".into(), json!(row.fired));
    o.insert("skipped".into(), json!(row.skipped));
    o.insert("last_error".into(), json!(row.last_error));
    o.insert("created_at".into(), json!(format_time(row.created_at_ms)));
    o.insert("updated_at".into(), json!(format_time(row.updated_at_ms)));
    out
}

// ----------------------------------------------------------------- engine

/// Body of `POST /v1/schedules`.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateSchedule {
    pub schedule_id: String,
    #[serde(default)]
    pub paused: bool,
    #[serde(flatten)]
    pub spec: Value,
}

/// The result of firing a schedule now (`POST /v1/schedules/{id}/trigger`).
#[derive(Debug, Clone, Serialize)]
pub struct Triggered {
    pub schedule_id: String,
    pub started: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
    /// Why nothing started (`overlap`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
}

fn not_found(id: &str) -> EngineError {
    EngineError::new(
        404,
        "SCHEDULE_NOT_FOUND",
        format!("schedule `{id}` not found"),
    )
}

fn terminal(status: &str) -> bool {
    !matches!(status, "RUNNING" | "COMPENSATING")
}

impl Engine {
    fn parse_spec(&self, spec: Value) -> Result<(ScheduleSpec, Plan)> {
        let mut spec: ScheduleSpec =
            serde_json::from_value(spec).map_err(|e| invalid(format!("invalid schedule: {e}")))?;
        let plan = spec.plan()?;
        let def = self
            .definition(&spec.workflow, spec.version.as_deref())
            .ok_or_else(|| {
                EngineError::new(
                    404,
                    "WORKFLOW_NOT_FOUND",
                    match &spec.version {
                        Some(v) => format!("workflow {}@{v} is not registered", spec.workflow),
                        None => format!("workflow {} is not registered", spec.workflow),
                    },
                )
            })?;
        if spec.input.is_null() {
            spec.input = json!({});
        }
        let mut checked = spec.input.clone();
        def.input.apply_defaults(&mut checked);
        def.input.validate(&checked, true).map_err(|e| {
            EngineError::new(
                400,
                "INVALID_INPUT",
                format!(
                    "the schedule input is invalid for {}@{} at {e}",
                    def.name, def.version
                ),
            )
        })?;
        Ok((spec, plan))
    }

    fn storage_err(e: crate::store::StoreError) -> EngineError {
        EngineError::new(500, "STORAGE_ERROR", e.to_string())
    }

    fn require_writable(&self) -> Result<()> {
        if self.store().read_only() {
            return Err(EngineError::new(
                503,
                "NOT_LEADER",
                "this server is a standby: schedules are changed on the leader",
            ));
        }
        Ok(())
    }

    /// Create a schedule. Its first occurrence is the first one after now.
    pub fn create_schedule(&mut self, req: CreateSchedule) -> Result<Value> {
        self.require_writable()?;
        if !valid_schedule_id(&req.schedule_id) {
            return Err(invalid(
                "schedule ids may contain letters, digits and - _ . (at most 100 characters)",
            ));
        }
        let (spec, plan) = self.parse_spec(req.spec)?;
        let now = self.now();
        let next = plan.next_after(now);
        let row = ScheduleRow {
            schedule_id: req.schedule_id.clone(),
            spec: serde_json::to_string(&spec).expect("spec serialises"),
            paused: req.paused,
            next_fire_ms: next,
            due_ms: next.map(|n| n + plan.jitter_for(&req.schedule_id, n)),
            last_fire_ms: None,
            last_execution_id: None,
            fired: 0,
            skipped: 0,
            last_error: None,
            created_at_ms: now,
            updated_at_ms: now,
        };
        if !self
            .store()
            .insert_schedule(&row)
            .map_err(Self::storage_err)?
        {
            return Err(EngineError::new(
                409,
                "SCHEDULE_EXISTS",
                format!("schedule `{}` already exists", req.schedule_id),
            ));
        }
        self.schedules_dirty = true;
        tracing::info!(schedule_id = %row.schedule_id, workflow = %spec.workflow, next = ?next.map(format_time), "schedule created");
        Ok(view(&row))
    }

    pub fn schedule_row(&self, id: &str) -> Result<ScheduleRow> {
        self.store()
            .schedule(id)
            .map_err(Self::storage_err)?
            .ok_or_else(|| not_found(id))
    }

    pub fn get_schedule(&self, id: &str) -> Result<Value> {
        Ok(view(&self.schedule_row(id)?))
    }

    /// The workflow a schedule starts (for authorization).
    pub fn schedule_workflow(&self, id: &str) -> Result<String> {
        let row = self.schedule_row(id)?;
        Ok(serde_json::from_str::<Value>(&row.spec)
            .ok()
            .and_then(|v| v["workflow"].as_str().map(str::to_string))
            .unwrap_or_default())
    }

    pub fn list_schedules(&self) -> Result<Vec<Value>> {
        Ok(self
            .store()
            .schedules()
            .map_err(Self::storage_err)?
            .iter()
            .map(view)
            .collect())
    }

    /// Change a schedule: `patch` holds the fields to change (`null` clears
    /// an optional one; `paused` pauses or resumes). The next occurrence is
    /// recomputed from now, so occurrences missed while paused are skipped.
    pub fn update_schedule(&mut self, id: &str, patch: Value) -> Result<Value> {
        self.require_writable()?;
        let Value::Object(patch) = patch else {
            return Err(invalid("expected a JSON object"));
        };
        let mut row = self.schedule_row(id)?;
        let mut spec: Value = serde_json::from_str(&row.spec).unwrap_or_else(|_| json!({}));
        for (k, v) in patch {
            match k.as_str() {
                "schedule_id" => {
                    if v.as_str() != Some(id) {
                        return Err(invalid("a schedule's id cannot be changed"));
                    }
                }
                "paused" => {
                    row.paused = v
                        .as_bool()
                        .ok_or_else(|| invalid("`paused` must be true or false"))?
                }
                _ if v.is_null() => {
                    spec.as_object_mut().expect("object").remove(&k);
                }
                _ => {
                    spec.as_object_mut().expect("object").insert(k, v);
                }
            }
        }
        // Switching between cron and interval: the new one replaces the old.
        let (spec, plan) = self.parse_spec(spec)?;
        let now = self.now();
        let next = plan.next_after(now);
        row.spec = serde_json::to_string(&spec).expect("spec serialises");
        row.next_fire_ms = next;
        row.due_ms = next.map(|n| n + plan.jitter_for(id, n));
        row.last_error = None;
        row.updated_at_ms = now;
        if !self
            .store()
            .update_schedule(&row)
            .map_err(Self::storage_err)?
        {
            return Err(not_found(id));
        }
        self.schedules_dirty = true;
        tracing::info!(schedule_id = %id, paused = row.paused, next = ?next.map(format_time), "schedule updated");
        Ok(view(&row))
    }

    pub fn set_schedule_paused(&mut self, id: &str, paused: bool) -> Result<Value> {
        self.update_schedule(id, json!({ "paused": paused }))
    }

    pub fn delete_schedule(&mut self, id: &str) -> Result<()> {
        self.require_writable()?;
        if !self
            .store()
            .delete_schedule(id)
            .map_err(Self::storage_err)?
        {
            return Err(not_found(id));
        }
        tracing::info!(schedule_id = %id, "schedule deleted");
        Ok(())
    }

    /// Fire a schedule now, outside its timing (the overlap policy applies).
    /// The execution id is `<schedule-id>:manual:<now>`.
    pub fn trigger_schedule(&mut self, id: &str) -> Result<Triggered> {
        self.require_writable()?;
        let row = self.schedule_row(id)?;
        let spec: ScheduleSpec = serde_json::from_str(&row.spec)
            .map_err(|e| invalid(format!("stored schedule is unreadable: {e}")))?;
        let now = self.now();
        let exec_id = format!("{id}:manual:{}", format_time(now));
        match self.fire_policy(&spec, id, row.last_execution_id.as_deref())? {
            false => {
                self.metrics.schedules_skipped_overlap += 1;
                Ok(Triggered {
                    schedule_id: id.to_string(),
                    started: false,
                    execution_id: None,
                    skipped: Some("overlap".into()),
                })
            }
            true => {
                let r = self.start_scheduled(
                    &spec.workflow,
                    spec.version.as_deref(),
                    spec.input.clone(),
                    exec_id,
                    id,
                )?;
                self.store()
                    .record_schedule_trigger(id, &r.execution_id, now)
                    .map_err(Self::storage_err)?;
                if r.created {
                    self.metrics.schedules_fired += 1;
                }
                tracing::info!(schedule_id = %id, execution_id = %r.execution_id, "schedule triggered");
                Ok(Triggered {
                    schedule_id: id.to_string(),
                    started: true,
                    execution_id: Some(r.execution_id),
                    skipped: None,
                })
            }
        }
    }

    /// Apply the overlap policy before a start: false means skip. With
    /// `cancel_previous`, a running previous execution is cancelled here.
    fn fire_policy(&mut self, spec: &ScheduleSpec, id: &str, last: Option<&str>) -> Result<bool> {
        if spec.overlap == Overlap::Allow {
            return Ok(true);
        }
        let Some(last) = last else { return Ok(true) };
        let running = self
            .store()
            .execution_status(last)
            .map_err(Self::storage_err)?
            .is_some_and(|s| !terminal(&s));
        if !running {
            return Ok(true);
        }
        match spec.overlap {
            Overlap::Skip => {
                tracing::info!(schedule_id = %id, running = %last, "schedule occurrence skipped: the previous execution is still running");
                Ok(false)
            }
            _ => {
                tracing::info!(schedule_id = %id, cancelling = %last, "schedule: cancelling the previous execution (overlap cancel_previous)");
                match self.cancel(last, &format!("schedule `{id}`: replaced by the next occurrence (overlap cancel_previous)")) {
                    Ok(_) => {}
                    Err(e) if e.code == "EXECUTION_FINISHED" => {}
                    Err(e) => return Err(e),
                }
                Ok(true)
            }
        }
    }

    /// Fire every due occurrence (called from [`Engine::tick`]). Looks the
    /// store up at most every [`POLL_MS`], unless a schedule changed here.
    pub fn fire_schedules(&mut self) -> Result<usize> {
        if self.store().read_only() {
            return Ok(0);
        }
        let now = self.now();
        if now < self.schedule_poll_at && !self.schedules_dirty {
            return Ok(0);
        }
        self.schedule_poll_at = now + POLL_MS;
        self.schedules_dirty = false;
        let due = self.store().due_schedules(now).map_err(Self::storage_err)?;
        let mut handled = 0;
        for row in due {
            match self.fire_one(&row, now) {
                Ok(true) => handled += 1,
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(schedule_id = %row.schedule_id, error = %e, "firing a schedule failed")
                }
            }
        }
        Ok(handled)
    }

    /// Claim, fire and advance one due schedule. False if another server
    /// holds the occurrence.
    fn fire_one(&mut self, row: &ScheduleRow, now: u64) -> Result<bool> {
        let id = row.schedule_id.as_str();
        let Some(occurrence) = row.next_fire_ms else {
            return Ok(false);
        };
        let node = self.instance_id().to_string();
        if !self
            .store()
            .claim_schedule(id, occurrence, &node, now, now + CLAIM_MS)
            .map_err(Self::storage_err)?
        {
            return Ok(false);
        }
        let parsed = serde_json::from_str::<ScheduleSpec>(&row.spec)
            .map_err(|e| invalid(e.to_string()))
            .and_then(|s| s.plan().map(|p| (s, p)));
        let (spec, plan) = match parsed {
            Ok(x) => x,
            Err(e) => {
                let _ =
                    self.store()
                        .release_schedule_claim(id, &node, now + CLAIM_MS, &e.message, now);
                return Err(e);
            }
        };
        // Every occurrence up to now: the last one is the candidate, the
        // earlier ones were missed.
        let mut latest = occurrence;
        let mut missed = 0u64;
        while missed < MAX_MISSED {
            match plan.next_after(latest) {
                Some(n) if n <= now => {
                    missed += 1;
                    latest = n;
                }
                _ => break,
            }
        }
        let late = now.saturating_sub(latest + plan.jitter_for(id, latest));
        let candidate = match spec.catchup {
            Catchup::RunOnce => Some(latest),
            Catchup::SkipMissed if late <= MISSED_AFTER_MS => Some(latest),
            Catchup::SkipMissed => {
                missed += 1;
                None
            }
        };
        let mut record = FireRecord {
            next_fire_ms: plan.next_after(latest.max(now)),
            last_fire_ms: Some(latest),
            skipped: missed,
            ..FireRecord::default()
        };
        record.due_ms = record.next_fire_ms.map(|n| n + plan.jitter_for(id, n));
        let mut started = None;
        let mut skipped_overlap = false;
        if let Some(at) = candidate {
            let go = match self.fire_policy(&spec, id, row.last_execution_id.as_deref()) {
                Ok(go) => go,
                Err(e) => {
                    let _ = self.store().release_schedule_claim(
                        id,
                        &node,
                        now + RETRY_MS,
                        &e.message,
                        now,
                    );
                    return Err(e);
                }
            };
            if go {
                let exec_id = fire_execution_id(id, at);
                match self.start_scheduled(
                    &spec.workflow,
                    spec.version.as_deref(),
                    spec.input.clone(),
                    exec_id,
                    id,
                ) {
                    Ok(r) => {
                        record.fired = 1;
                        record.last_execution_id = Some(r.execution_id.clone());
                        started = Some(r);
                    }
                    Err(e) => {
                        // Not advanced: the same occurrence is tried again.
                        let _ = self.store().release_schedule_claim(
                            id,
                            &node,
                            now + RETRY_MS,
                            &e.message,
                            now,
                        );
                        return Err(e);
                    }
                }
            } else {
                skipped_overlap = true;
                record.skipped += 1;
            }
        }
        let recorded = self
            .store()
            .finish_schedule_fire(id, occurrence, &node, now, &record)
            .map_err(Self::storage_err)?;
        if !recorded {
            tracing::warn!(schedule_id = %id, "schedule changed while it fired; its new settings apply from now on");
        }
        if let Some(r) = &started {
            if r.created {
                self.metrics.schedules_fired += 1;
            }
            tracing::info!(schedule_id = %id, execution_id = %r.execution_id, created = r.created, next = ?record.next_fire_ms.map(format_time), "schedule fired");
        }
        if skipped_overlap {
            self.metrics.schedules_skipped_overlap += 1;
        }
        let missed_count = if skipped_overlap {
            record.skipped - 1
        } else {
            record.skipped
        };
        if missed_count > 0 {
            self.metrics.schedules_skipped_missed += missed_count;
            tracing::warn!(schedule_id = %id, missed = missed_count, catchup = ?spec.catchup, "schedule occurrences were missed while no server could fire them");
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(t: &str) -> u64 {
        parse_time(t).unwrap()
    }

    fn next(cron: &str, tz: &str, after: &str) -> String {
        let c = Cron::parse(cron).unwrap();
        format_time(
            c.next_after(ms(after), &TimeZone::get(tz).unwrap())
                .unwrap(),
        )
    }

    #[test]
    fn cron_fields() {
        assert_eq!(
            next("0 2 * * *", "UTC", "2026-11-15T01:59:59Z"),
            "2026-11-15T02:00:00Z"
        );
        // Strictly after: an occurrence at exactly `t` is not returned.
        assert_eq!(
            next("0 2 * * *", "UTC", "2026-11-15T02:00:00Z"),
            "2026-11-16T02:00:00Z"
        );
        assert_eq!(
            next("*/15 * * * *", "UTC", "2026-11-15T10:07:00Z"),
            "2026-11-15T10:15:00Z"
        );
        assert_eq!(
            next("30 9 * * MON-FRI", "UTC", "2026-11-14T00:00:00Z"),
            "2026-11-16T09:30:00Z"
        );
        assert_eq!(
            next("0 0 1 JAN,jul *", "UTC", "2026-02-01T00:00:00Z"),
            "2026-07-01T00:00:00Z"
        );
        assert_eq!(
            next("0 12 * * 7", "UTC", "2026-11-15T13:00:00Z"),
            "2026-11-22T12:00:00Z"
        );
        assert_eq!(
            next("0 0 29 2 *", "UTC", "2026-01-01T00:00:00Z"),
            "2028-02-29T00:00:00Z"
        );
        // Both day fields restricted: either matches (the 13th, or a Friday).
        assert_eq!(
            next("0 0 13 * 5", "UTC", "2026-11-01T00:00:00Z"),
            "2026-11-06T00:00:00Z"
        );
        assert_eq!(
            next("@hourly", "UTC", "2026-11-15T10:00:00Z"),
            "2026-11-15T11:00:00Z"
        );
        assert_eq!(
            next("5/20 * * * *", "UTC", "2026-11-15T10:06:00Z"),
            "2026-11-15T10:25:00Z"
        );
    }

    #[test]
    fn cron_time_zones_and_daylight_saving() {
        // 02:00 in Berlin is 01:00 UTC in winter.
        assert_eq!(
            next("0 2 * * *", "Europe/Berlin", "2026-11-15T00:00:00Z"),
            "2026-11-15T01:00:00Z"
        );
        // 2026-03-29: 02:30 does not exist in Berlin; it fires at 03:30 CEST.
        assert_eq!(
            next("30 2 * * *", "Europe/Berlin", "2026-03-29T00:00:00Z"),
            "2026-03-29T01:30:00Z"
        );
        // 2026-10-25: 02:30 happens twice; it fires once, at the first.
        let c = Cron::parse("30 2 * * *").unwrap();
        let tz = TimeZone::get("Europe/Berlin").unwrap();
        let first = c.next_after(ms("2026-10-24T12:00:00Z"), &tz).unwrap();
        assert_eq!(format_time(first), "2026-10-25T00:30:00Z");
        assert_eq!(
            format_time(c.next_after(first, &tz).unwrap()),
            "2026-10-26T01:30:00Z"
        );
    }

    #[test]
    fn cron_errors() {
        for (bad, msg) in [
            ("0 2 * *", "expected 5 fields"),
            ("60 * * * *", "out of range"),
            ("* 24 * * *", "out of range"),
            ("* * 0 * *", "out of range"),
            ("* * * FOO *", "not a valid month"),
            ("*/0 * * * *", "at least 1"),
            ("@often", "unknown macro"),
        ] {
            let e = Cron::parse(bad).unwrap_err();
            assert!(e.contains(msg), "{bad}: {e}");
        }
    }

    #[test]
    fn intervals_start_end_and_jitter() {
        let spec = |extra: Value| -> Plan {
            let mut v = json!({ "workflow": "W", "interval": "15m" });
            v.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            serde_json::from_value::<ScheduleSpec>(v)
                .unwrap()
                .plan()
                .unwrap()
        };
        let p = spec(json!({}));
        assert_eq!(
            format_time(p.next_after(ms("2026-11-15T10:07:00Z")).unwrap()),
            "2026-11-15T10:15:00Z"
        );
        let p =
            spec(json!({ "start_at": "2026-11-15T10:01:00Z", "end_at": "2026-11-15T10:40:00Z" }));
        let all: Vec<String> = p.upcoming(0, 10).into_iter().map(format_time).collect();
        assert_eq!(
            all,
            [
                "2026-11-15T10:01:00Z",
                "2026-11-15T10:16:00Z",
                "2026-11-15T10:31:00Z"
            ]
        );
        let p = spec(json!({ "jitter": "30s" }));
        let j = p.jitter_for("s", 1000);
        assert!(j < 30_000);
        assert_eq!(j, p.jitter_for("s", 1000));
        assert_eq!(parse_duration("1h30m").unwrap(), 5_400_000);
        assert!(parse_duration("15").is_err());
        assert!(parse_duration("5w").is_err());
        let bad = |v: Value| {
            serde_json::from_value::<ScheduleSpec>(v)
                .unwrap()
                .plan()
                .unwrap_err()
                .message
        };
        assert!(bad(json!({ "workflow": "W" })).contains("needs `cron` or `interval`"));
        assert!(bad(json!({ "workflow": "W", "interval": "500ms" })).contains("at least 1s"));
        assert!(
            bad(json!({ "workflow": "W", "cron": "* * * * *", "timezone": "Mars/Base" }))
                .contains("unknown time zone")
        );
    }
}
