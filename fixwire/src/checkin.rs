//! Cron monitors (`/v1/check-ins/{monitor}`) and feedback (`/v1/feedback`).

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use crate::client::{Client, new_id};
use crate::hub::Hub;
use crate::transport::Category;

/// How a scheduled job's run is going.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckInStatus {
    /// It started.
    InProgress,
    /// It ended well.
    Ok,
    /// It failed.
    Error,
}

impl CheckInStatus {
    fn as_str(self) -> &'static str {
        match self {
            CheckInStatus::InProgress => "in_progress",
            CheckInStatus::Ok => "ok",
            CheckInStatus::Error => "error",
        }
    }
}

/// When a job runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MonitorSchedule {
    /// A crontab, such as `0 3 * * *`.
    Crontab(String),
    /// Every `n` units: minute, hour, day, week, month or year.
    Interval(u32, String),
}

/// Creates or updates the monitor a check-in is for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorConfig {
    /// When the job runs.
    pub schedule: MonitorSchedule,
    /// The minutes a check-in may be late.
    pub checkin_margin: Option<u32>,
    /// The minutes a run may take.
    pub max_runtime: Option<u32>,
    /// The schedule's time zone, such as `Europe/Berlin`.
    pub timezone: Option<String>,
}

impl MonitorConfig {
    /// A monitor for a job on a crontab.
    pub fn crontab(crontab: impl Into<String>) -> MonitorConfig {
        MonitorConfig {
            schedule: MonitorSchedule::Crontab(crontab.into()),
            checkin_margin: None,
            max_runtime: None,
            timezone: None,
        }
    }

    fn to_json(&self) -> Value {
        let schedule = match &self.schedule {
            MonitorSchedule::Crontab(c) => json!({"type": "crontab", "value": c}),
            MonitorSchedule::Interval(n, unit) => {
                json!({"type": "interval", "value": n, "unit": unit})
            }
        };
        let mut config = json!({"schedule": schedule});
        if let Some(m) = self.checkin_margin {
            config["checkin_margin"] = m.into();
        }
        if let Some(m) = self.max_runtime {
            config["max_runtime"] = m.into();
        }
        if let Some(tz) = &self.timezone {
            config["timezone"] = tz.clone().into();
        }
        config
    }
}

/// A run of a scheduled job, reported to its monitor.
#[derive(Clone, Debug)]
pub struct CheckIn {
    /// The monitor's slug.
    pub monitor: String,
    /// How the run is going.
    pub status: CheckInStatus,
    /// Ties the end of a run to its start; made when `None`.
    pub id: Option<String>,
    /// How long the run took.
    pub duration: Option<Duration>,
    /// Creates or updates the monitor.
    pub config: Option<MonitorConfig>,
}

impl Client {
    /// Reports a run of a scheduled job: `InProgress` when it starts, then
    /// `Ok` or `Error` with the returned id. Its id, or `None` when it was
    /// not sent.
    pub fn capture_check_in(&self, check_in: CheckIn) -> Option<String> {
        if !self.is_enabled() || check_in.monitor.trim().is_empty() {
            return None;
        }
        let id = check_in.id.unwrap_or_else(|| new_id(16));
        let mut body = json!({
            "sdk": crate::sdk(), "check_in_id": id, "status": check_in.status.as_str(),
            "environment": self.options().environment,
        });
        if let Some(d) = check_in.duration {
            body["duration"] = d.as_secs_f64().into();
        }
        if let Some(c) = &check_in.config {
            body["monitor_config"] = c.to_json();
        }
        let path = format!("/v1/check-ins/{}", percent_encode(&check_in.monitor));
        self.send_json(&path, Category::CheckIn, &body)
            .then_some(id)
    }
}

/// A monitor's slug in a URL path.
fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Runs `job` as a run of the monitor: `InProgress`, then `Ok`, or `Error`
/// when it returns an error or panics (the panic goes on).
///
/// ```no_run
/// let report = fixwire::with_monitor("nightly-report", Some(fixwire::MonitorConfig::crontab("0 3 * * *")), || {
///     Ok::<_, std::io::Error>(())
/// });
/// ```
pub fn with_monitor<T, E>(
    monitor: &str,
    config: Option<MonitorConfig>,
    job: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    struct End<'a> {
        client: Option<std::sync::Arc<Client>>,
        monitor: &'a str,
        id: Option<String>,
        start: Instant,
        ok: bool,
    }
    impl Drop for End<'_> {
        fn drop(&mut self) {
            if let (Some(client), Some(id)) = (&self.client, self.id.take()) {
                // A run is always measured: where the clock is coarser than the job, it took less
                // than a tick, not nothing.
                let took = self.start.elapsed().max(Duration::from_nanos(1));
                let status = if self.ok && !std::thread::panicking() {
                    CheckInStatus::Ok
                } else {
                    CheckInStatus::Error
                };
                client.capture_check_in(CheckIn {
                    monitor: self.monitor.into(),
                    status,
                    id: Some(id),
                    duration: Some(took),
                    config: None,
                });
            }
        }
    }
    let client = Hub::current().client();
    let id = client.as_ref().and_then(|c| {
        c.capture_check_in(CheckIn {
            monitor: monitor.into(),
            status: CheckInStatus::InProgress,
            id: None,
            duration: None,
            config,
        })
    });
    let mut end = End {
        client,
        monitor,
        id,
        start: Instant::now(),
        ok: false,
    };
    let result = job();
    end.ok = result.is_ok();
    result
}

/// What someone said about an error or an AI answer: a message, a score
/// from -1 (bad) to 1 (good), or both.
#[derive(Clone, Debug, Default)]
pub struct Feedback {
    /// What they said.
    pub message: Option<String>,
    /// From -1 (bad) to 1 (good).
    pub score: Option<f64>,
    /// Ties it to a trace or an agent run (a negative score opens a
    /// `user_feedback` issue); the current span's trace when `None`.
    pub trace_id: Option<String>,
    /// Ties it to an error.
    pub event_id: Option<String>,
    /// Their name; the scope's user's when `None`.
    pub name: Option<String>,
    /// Their email address; the scope's user's when `None`.
    pub email: Option<String>,
    /// The page or screen it came from.
    pub url: Option<String>,
    /// Where it came from (`api`, `widget`, …); `api` when `None`.
    pub source: Option<String>,
}

impl Hub {
    /// Sends feedback. Its id, or `None` when it holds neither a message nor
    /// a score.
    pub fn capture_feedback(&self, f: Feedback) -> Option<String> {
        let client = self.client().filter(|c| c.is_enabled())?;
        let message = f
            .message
            .map(|m| m.trim().to_owned())
            .filter(|m| !m.is_empty());
        let score = f
            .score
            .filter(|s| s.is_finite())
            .map(|s| s.clamp(-1.0, 1.0))
            .filter(|s| *s != 0.0);
        if message.is_none() && score.is_none() {
            return None;
        }
        let (user, span) = self.configure_scope(|s| (s.user.clone(), s.span.clone()));
        let id = new_id(16);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as f64
            / 1000.0;
        let mut body = Map::new();
        let mut put = |k: &str, v: Option<String>| {
            if let Some(v) = v.filter(|v| !v.is_empty()) {
                body.insert(k.into(), v.into());
            }
        };
        put("message", message);
        put(
            "trace_id",
            f.trace_id.or_else(|| span.map(|s| s.trace_id().to_owned())),
        );
        put("event_id", f.event_id);
        put(
            "name",
            f.name
                .or_else(|| user.as_ref().and_then(|u| u.username.clone())),
        );
        put(
            "email",
            f.email
                .or_else(|| user.as_ref().and_then(|u| u.email.clone())),
        );
        put("url", f.url);
        put("release", client.options().release.clone());
        if let Some(s) = score {
            body.insert("score".into(), s.into());
        }
        body.insert("sdk".into(), crate::sdk());
        body.insert("feedback_id".into(), id.clone().into());
        body.insert("timestamp".into(), timestamp.into());
        body.insert(
            "source".into(),
            f.source.unwrap_or_else(|| "api".into()).into(),
        );
        body.insert(
            "environment".into(),
            client.options().environment.clone().into(),
        );
        let skip = [
            "sdk",
            "feedback_id",
            "timestamp",
            "event_id",
            "trace_id",
            "release",
            "environment",
            "source",
        ];
        let body = Value::Object(client.scrub(body, &skip));
        client
            .send_json("/v1/feedback", Category::Feedback, &body)
            .then_some(id)
    }
}
