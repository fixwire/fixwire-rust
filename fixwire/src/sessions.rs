//! Release health for servers: each request is a session, counted per
//! minute and user and sent about every minute (`fixwire-protocol` §5).

use std::collections::HashMap;
use std::sync::mpsc::{RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::hub::{Hub, lock};
use crate::transport::{Category, Request, Transport};
use crate::types::User;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Ok,
    Errored,
    Crashed,
}

/// The session of the request a scope serves.
#[derive(Debug)]
pub(crate) struct RequestSession {
    status: Mutex<Status>,
}

impl RequestSession {
    /// Errored, or crashed when nothing handled the error.
    pub(crate) fn mark(&self, crashed: bool) {
        let mut status = lock(&self.status);
        if crashed {
            *status = Status::Crashed;
        } else if *status == Status::Ok {
            *status = Status::Errored;
        }
    }
}

/// Ends a request's session when dropped; see `Hub::start_request_session`.
#[derive(Debug)]
pub struct RequestSessionGuard {
    hub: Hub,
    session: Option<Arc<RequestSession>>,
}

impl Hub {
    /// Starts the session of the request the hub's scope serves: it ends,
    /// and is counted, when the guard is dropped. The tower layer does this.
    pub fn start_request_session(&self) -> RequestSessionGuard {
        let on = self.client().is_some_and(|c| c.sessions.is_some());
        let session = on.then(|| {
            Arc::new(RequestSession {
                status: Mutex::new(Status::Ok),
            })
        });
        if let Some(s) = &session {
            self.configure_scope(|scope| scope.session = Some(Arc::clone(s)));
        }
        RequestSessionGuard {
            hub: self.clone(),
            session,
        }
    }
}

impl Drop for RequestSessionGuard {
    fn drop(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        let user = self.hub.configure_scope(|s| s.user.clone());
        let status = *lock(&session.status);
        if let Some(sessions) = self.hub.client().as_ref().and_then(|c| c.sessions.as_ref()) {
            sessions.record(status, device_id(user.as_ref()), SystemTime::now());
        }
    }
}

/// The user, hashed on the device: the first 16 bytes of the SHA-256 of
/// their id (else email, else name), as hex. Never the raw id.
pub(crate) fn device_id(user: Option<&User>) -> String {
    let Some(id) = user.and_then(|u| u.id.as_ref().or(u.email.as_ref()).or(u.username.as_ref()))
    else {
        return String::new();
    };
    Sha256::digest(id.as_bytes())[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The (minute, user) counts kept between sends; past them, users are
/// counted without their id, so memory and the body (at most 1 MB) stay
/// small however many users come.
const MAX_BUCKETS: usize = 5000;
/// The aggregates of one request (those without a user, one a minute, may
/// pass `MAX_BUCKETS`).
const MAX_AGGREGATES: usize = 5000;

#[derive(Default)]
struct Counts {
    exited: u64,
    errored: u64,
    crashed: u64,
}

/// Counts request sessions per minute and user, and sends them about every
/// interval.
pub(crate) struct Aggregates {
    buckets: Arc<Mutex<HashMap<(u64, String), Counts>>>,
    stop: Mutex<Option<SyncSender<()>>>,
    transport: Arc<Transport>,
    release: String,
    environment: String,
}

impl Aggregates {
    pub(crate) fn start(
        transport: Arc<Transport>,
        release: String,
        environment: String,
        interval: Duration,
    ) -> Arc<Aggregates> {
        let (stop, stopped) = sync_channel::<()>(1);
        let a = Arc::new(Aggregates {
            buckets: Arc::default(),
            stop: Mutex::new(Some(stop)),
            transport,
            release,
            environment,
        });
        let weak = Arc::downgrade(&a);
        let _ = std::thread::Builder::new()
            .name("fixwire-sessions".into())
            .spawn(move || {
                while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(interval) {
                    match weak.upgrade() {
                        Some(a) => a.send(),
                        None => break,
                    }
                }
            });
        a
    }

    fn record(&self, status: Status, did: String, at: SystemTime) {
        let minute = at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() / 60 * 60;
        let mut buckets = lock(&self.buckets);
        let mut key = (minute, did);
        if buckets.len() >= MAX_BUCKETS && !buckets.contains_key(&key) {
            key.1 = String::new();
        }
        let counts = buckets.entry(key).or_default();
        match status {
            Status::Ok => counts.exited += 1,
            Status::Errored => counts.errored += 1,
            Status::Crashed => counts.crashed += 1,
        }
    }

    /// Sends what was counted.
    pub(crate) fn send(&self) {
        for body in self.take() {
            if let Ok(body) = serde_json::to_vec(&body) {
                self.transport
                    .send(Request::json("/v1/sessions", Category::Session, body));
            }
        }
    }

    /// What was counted, as bodies of at most 5000 aggregates.
    fn take(&self) -> Vec<Value> {
        let buckets = std::mem::take(&mut *lock(&self.buckets));
        let mut aggregates: Vec<_> = buckets.into_iter().collect();
        aggregates.sort_by(|a, b| a.0.cmp(&b.0));
        let aggregates: Vec<_> = aggregates
            .into_iter()
            .map(|((minute, did), c)| {
                let mut a = json!({"started": rfc3339(minute), "exited": c.exited, "errored": c.errored, "crashed": c.crashed});
                if !did.is_empty() {
                    a["did"] = did.into();
                }
                a
            })
            .collect();
        aggregates
            .chunks(MAX_AGGREGATES)
            .map(|part| json!({"sdk": crate::sdk(), "release": self.release, "environment": self.environment, "aggregates": part}))
            .collect()
    }

    pub(crate) fn stop(&self) {
        if let Some(stop) = lock(&self.stop).take() {
            let _ = stop.try_send(());
        }
    }
}

/// Unix seconds as RFC 3339 in UTC (`2026-10-05T10:04:00Z`).
pub(crate) fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_utc_times() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_791_190_800), "2026-10-05T09:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn hashes_users_on_the_device() {
        let u = User::with_id("user-1");
        assert_eq!(device_id(Some(&u)).len(), 32);
        assert_ne!(device_id(Some(&u)), "user-1");
        assert_eq!(device_id(None), "");
    }

    #[test]
    fn many_users_are_counted_in_bounded_memory() {
        let transport = Arc::new(Transport::new(
            "http://k@127.0.0.1:9".parse().unwrap(),
            &crate::options::Options::default(),
        ));
        let a = Aggregates::start(
            transport,
            "shop@1.0.0".into(),
            "production".into(),
            Duration::from_secs(3600),
        );
        let at = UNIX_EPOCH + Duration::from_secs(1_791_190_800);
        for i in 0..MAX_BUCKETS + 100 {
            let user = User::with_id(i.to_string());
            a.record(Status::Ok, device_id(Some(&user)), at);
        }
        a.stop();
        {
            let buckets = lock(&a.buckets);
            assert_eq!(buckets.len(), MAX_BUCKETS + 1);
            assert_eq!(
                buckets[&(1_791_190_800, String::new())].exited,
                100,
                "the rest counted without their id"
            );
        }
        // 5001 aggregates: two requests.
        let bodies = a.take();
        let sizes: Vec<usize> = bodies
            .iter()
            .map(|b| b["aggregates"].as_array().unwrap().len())
            .collect();
        assert_eq!(sizes, [MAX_AGGREGATES, 1]);
        assert!(a.take().is_empty());
    }
}
