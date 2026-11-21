//! Time in the fixtures and on the fake server.
//!
//! The recorder stores every timestamp relative to [`FIXTURE_NOW`]: an object created three
//! hours before the recording is stored as `FIXTURE_NOW - 3h`. The server shifts them by
//! `now - FIXTURE_NOW` on the way out, so ages look the same whenever the tests run, and it
//! renders `Age` cells from the shifted creation time.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use k8s_openapi::jiff::Timestamp;
use serde_json::Value;

/// The instant the fixtures were recorded at, in fixture time.
pub const FIXTURE_NOW: &str = "2026-01-01T00:00:00Z";

/// [`FIXTURE_NOW`] in Unix seconds.
pub const FIXTURE_NOW_SECONDS: i64 = 1_767_225_600;

/// For an RFC 3339 UTC timestamp (`2026-11-19T10:00:00Z`, `...00.123456Z`): its Unix seconds
/// and the number of fractional digits.
pub fn parse(text: &str) -> Option<(i64, usize)> {
    let b = text.as_bytes();
    let shape = b"dddd-dd-ddTdd:dd:dd";
    if b.len() < shape.len() + 1 || *b.last()? != b'Z' {
        return None;
    }
    let head_ok = shape.iter().zip(b).all(|(p, c)| match p {
        b'd' => c.is_ascii_digit(),
        p => p == c,
    });
    let digits = match &b[shape.len()..b.len() - 1] {
        [] => 0,
        [b'.', digits @ ..] if !digits.is_empty() && digits.iter().all(u8::is_ascii_digit) => {
            digits.len()
        }
        _ => return None,
    };
    if !head_ok {
        return None;
    }
    let seconds = text[..shape.len()].to_owned() + "Z";
    Some((seconds.parse::<Timestamp>().ok()?.as_second(), digits))
}

/// Formats Unix seconds as RFC 3339 UTC with `digits` fractional zeros.
pub fn format(seconds: i64, digits: usize) -> String {
    let text = Timestamp::from_second(seconds)
        .map(|t| t.to_string())
        .unwrap_or_else(|_| FIXTURE_NOW.to_owned());
    match digits {
        0 => text,
        n => format!("{}.{}Z", text.trim_end_matches('Z'), "0".repeat(n)),
    }
}

/// Rounds an age so re-recordings of an unchanged cluster give the same value: to 10 s under a
/// minute, to the minute under an hour, to the hour under a day, else to the day.
pub fn round_age(seconds: i64) -> i64 {
    let a = seconds.abs();
    let unit = match a {
        0..60 => 10,
        60..3600 => 60,
        3600..86_400 => 3600,
        _ => 86_400,
    };
    seconds.signum() * ((a + unit / 2) / unit * unit)
}

/// Unix seconds of a system time.
pub fn unix(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Calls `f` on every timestamp string in `value` (seconds and fractional digits), replacing it
/// with what `f` returns.
pub fn map_timestamps(value: &mut Value, f: &impl Fn(i64) -> i64) {
    match value {
        Value::String(s) => {
            if let Some((seconds, digits)) = parse(s) {
                *s = format(f(seconds), digits);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| map_timestamps(v, f)),
        Value::Object(map) => map.values_mut().for_each(|v| map_timestamps(v, f)),
        _ => {}
    }
}

/// How far a clock has run since its start. Tests move it forward or stop it.
#[derive(Debug)]
struct Elapsed {
    /// Time counted up to `running_since`, with every advance.
    counted: Duration,
    /// When the clock last started running; `None` while frozen.
    running_since: Option<Instant>,
}

impl Elapsed {
    fn get(&self) -> Duration {
        self.counted
            + self
                .running_since
                .map_or(Duration::ZERO, |since| since.elapsed())
    }
}

/// The server's clock: fixture time shifted to the server's start. Clones share one time, so
/// advancing or freezing it reaches logs and open watches too.
#[derive(Debug, Clone)]
pub struct Clock {
    /// Seconds added to every fixture timestamp.
    shift: i64,
    elapsed: Arc<Mutex<Elapsed>>,
    now_at_start: i64,
}

impl Clock {
    /// A clock on which fixture time [`FIXTURE_NOW`] is `now`.
    pub fn new(now: SystemTime) -> Self {
        let now_at_start = unix(now);
        Clock {
            shift: now_at_start - FIXTURE_NOW_SECONDS,
            elapsed: Arc::new(Mutex::new(Elapsed {
                counted: Duration::ZERO,
                running_since: Some(Instant::now()),
            })),
            now_at_start,
        }
    }

    fn elapsed(&self) -> std::sync::MutexGuard<'_, Elapsed> {
        self.elapsed.lock().expect("fake API clock lock")
    }

    /// Moves the time forward by `by`, whether the clock is running or frozen.
    pub fn advance(&self, by: Duration) {
        self.elapsed().counted += by;
    }

    /// Stops the time until [`Clock::resume`]. Freezing a frozen clock changes nothing.
    pub fn freeze(&self) {
        let mut elapsed = self.elapsed();
        elapsed.counted = elapsed.get();
        elapsed.running_since = None;
    }

    /// Starts a frozen clock again from the time it shows. A running clock is left alone.
    pub fn resume(&self) {
        self.elapsed()
            .running_since
            .get_or_insert_with(Instant::now);
    }

    /// The server's current time in Unix seconds.
    pub fn now(&self) -> i64 {
        self.now_at_start + i64::try_from(self.elapsed().get().as_secs()).unwrap_or(0)
    }

    /// The server's current time, to the nanosecond (log line timestamps).
    pub fn now_precise(&self) -> Timestamp {
        let elapsed = self.elapsed().get();
        Timestamp::from_second(self.now_at_start)
            .ok()
            .and_then(|start| start.checked_add(elapsed).ok())
            .unwrap_or(Timestamp::UNIX_EPOCH)
    }

    /// The instant the server started (fixture time [`FIXTURE_NOW`] in the server's time).
    pub fn start(&self) -> Timestamp {
        Timestamp::from_second(self.now_at_start).unwrap_or(Timestamp::UNIX_EPOCH)
    }

    /// The server's current time in fixture time, as objects are stored: new objects take
    /// timestamps from here, and [`Clock::rebase`] shifts them to "now" on the way out.
    pub fn fixture_now(&self) -> String {
        format(self.now() - self.shift, 0)
    }

    /// Shifts every timestamp in a response from fixture time to the server's time, and renders
    /// `Age` cells of a Table from the rows' creation times.
    pub fn rebase(&self, value: &mut Value) {
        self.rebase_with_columns(value, &Value::Null);
    }

    /// [`Clock::rebase`] for a Table whose own `columnDefinitions` may be omitted (later watch
    /// events carry `null`): `columns` locates the `Age` column then, and the wire format stays.
    pub fn rebase_with_columns(&self, value: &mut Value, columns: &Value) {
        map_timestamps(value, &|s| s + self.shift);
        self.render_ages(value, columns);
        self.render_restarts(value);
    }

    /// Renders `"<n> (<age> ago)"` in the Restarts cells of rows that carry their last restart
    /// ([`super::FakeRow::with_last_restart`]), and drops that marker.
    fn render_restarts(&self, value: &mut Value) {
        let Some(rows) = value.get_mut("rows").and_then(Value::as_array_mut) else {
            return;
        };
        let now = self.now();
        for row in rows {
            let Some(marker) = row
                .as_object_mut()
                .and_then(|r| r.remove(super::row::LAST_RESTART))
            else {
                continue;
            };
            let at = marker["at"].as_str().and_then(parse);
            let index = marker["cell"]
                .as_u64()
                .and_then(|i| usize::try_from(i).ok());
            if let (Some((at, _)), Some(cell)) =
                (at, index.and_then(|i| row.get_mut("cells")?.get_mut(i)))
            {
                let count = cell.as_str().unwrap_or("0").to_owned();
                *cell = format!(
                    "{count} ({} ago)",
                    flint_core::table::human_duration(now - at)
                )
                .into();
            }
        }
    }

    fn render_ages(&self, value: &mut Value, fallback: &Value) {
        let age = value
            .get("columnDefinitions")
            .and_then(Value::as_array)
            .or_else(|| fallback.as_array())
            .and_then(|c| c.iter().position(|c| c["name"] == "Age"));
        let (Some(age), Some(rows)) = (age, value.get_mut("rows").and_then(Value::as_array_mut))
        else {
            return;
        };
        let now = self.now();
        for row in rows {
            let created = row
                .pointer("/object/metadata/creationTimestamp")
                .and_then(Value::as_str)
                .and_then(parse);
            if let (Some((created, _)), Some(cell)) =
                (created, row.get_mut("cells").and_then(|c| c.get_mut(age)))
            {
                *cell = flint_core::table::human_duration(now - created).into();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_and_formats_timestamps() {
        assert_eq!(parse(FIXTURE_NOW), Some((FIXTURE_NOW_SECONDS, 0)));
        assert_eq!(
            parse("2026-01-01T00:00:10.123456Z"),
            Some((FIXTURE_NOW_SECONDS + 10, 6))
        );
        assert_eq!(parse("2026-01-01T00:00:00"), None);
        assert_eq!(parse("2026-01-01T00:00:00.Z"), None);
        assert_eq!(parse("started 2026-01-01T00:00:00Z"), None);
        assert_eq!(format(FIXTURE_NOW_SECONDS, 0), FIXTURE_NOW);
        assert_eq!(
            format(FIXTURE_NOW_SECONDS - 60, 6),
            "2025-12-31T23:59:00.000000Z"
        );
    }

    #[test]
    fn ages_round_coarser_as_they_grow() {
        assert_eq!(round_age(14), 10);
        assert_eq!(round_age(16), 20);
        assert_eq!(round_age(125), 120);
        assert_eq!(round_age(3 * 3600 + 1700), 3 * 3600);
        assert_eq!(round_age(5 * 86_400 + 50_000), 6 * 86_400);
        assert_eq!(round_age(-125), -120);
    }

    #[test]
    fn a_frozen_clock_moves_only_when_advanced_and_clones_share_it() {
        let start = UNIX_EPOCH + Duration::from_secs(1_900_000_000);
        let clock = Clock::new(start);
        let shared = clock.clone();
        clock.freeze();
        let frozen = clock.now_precise();
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(clock.now_precise(), frozen);
        clock.freeze();
        assert_eq!(clock.now_precise(), frozen);

        clock.advance(Duration::from_secs(3600));
        assert_eq!(shared.now_precise(), frozen + Duration::from_secs(3600));
        assert_eq!(
            parse(&shared.fixture_now()).unwrap().0,
            FIXTURE_NOW_SECONDS + (shared.now() - 1_900_000_000)
        );
        // The start and the shift of fixture timestamps stay where they were.
        assert_eq!(clock.start().as_second(), 1_900_000_000);

        let before = clock.now_precise();
        clock.resume();
        clock.resume();
        std::thread::sleep(Duration::from_millis(5));
        let after = clock.now_precise();
        assert!(after > before, "{after} after {before}");
        assert!(after < before + Duration::from_secs(60), "{after} jumped");
    }

    #[test]
    fn rebases_responses_to_the_server_start_and_renders_ages() {
        let start = UNIX_EPOCH + Duration::from_secs(1_900_000_000);
        let clock = Clock::new(start);
        let mut table = json!({
            "columnDefinitions": [{"name": "Name"}, {"name": "Age"}],
            "rows": [{"cells": ["web", "1d"], "object": {"metadata": {
                "creationTimestamp": format(FIXTURE_NOW_SECONDS - 3 * 3600, 0)}}}],
            "eventTime": "2026-01-01T00:00:00.000000Z",
        });
        clock.rebase(&mut table);
        assert_eq!(table["rows"][0]["cells"][1], "3h");
        assert_eq!(
            table["rows"][0]["object"]["metadata"]["creationTimestamp"],
            format(1_900_000_000 - 3 * 3600, 0)
        );
        assert_eq!(table["eventTime"], format(1_900_000_000, 6));
        assert!(clock.now() >= 1_900_000_000);
        // A restart five minutes before the start, rendered relative to now.
        let mut rows = json!({"rows": [{"cells": ["web", "2"], "fakeLastRestart": {
            "cell": 1, "at": format(FIXTURE_NOW_SECONDS - 300, 0)}}]});
        clock.rebase(&mut rows);
        assert_eq!(rows["rows"][0]["cells"][1], "2 (5m ago)");
        assert!(rows["rows"][0].get("fakeLastRestart").is_none());
        assert_eq!(clock.start().as_second(), 1_900_000_000);
        assert!(clock.now_precise() >= clock.start());
        // Fixture time runs from FIXTURE_NOW at the start.
        let (fixture_now, _) = parse(&clock.fixture_now()).unwrap();
        assert!((FIXTURE_NOW_SECONDS..FIXTURE_NOW_SECONDS + 60).contains(&fixture_now));
    }
}
