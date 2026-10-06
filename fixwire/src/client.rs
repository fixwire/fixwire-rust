//! The client: what is captured for one project goes through it, redacted
//! and budgeted, to the transport.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Map, Value};

use crate::budget::{Budget, issue_of};
use crate::dsn::{Dsn, InvalidDsn};
use crate::options::Options;
use crate::redaction::Redactor;
use crate::sessions::Aggregates;
use crate::transport::{Category, Request, Transport};
use crate::types::{Event, Level};

/// The most an error or a message may weigh (`sdks/PROTOCOL.md` §4).
const MAX_EVENT_BYTES: usize = 1 << 20;

/// Sends to one project. Most programs use the one `init` sets up, through
/// the crate's functions or a `Hub`.
pub struct Client {
    opts: Options,
    transport: Option<Arc<Transport>>,
    budget: Budget,
    redactor: Option<Redactors>,
    pub(crate) sessions: Option<Arc<Aggregates>>,
}

impl Client {
    /// A client for `opts`; without a DSN it is disabled and sends nothing.
    pub fn new(opts: Options) -> Result<Client, InvalidDsn> {
        let opts = opts.with_defaults();
        let dsn: Option<Dsn> = opts.dsn.as_deref().map(str::parse).transpose()?;
        let redactor = (dsn.is_some() && opts.redact).then(|| match &opts.sensitive_keys {
            None => Redactors::Shared(Redactor::default_shared()),
            Some(keys) => Redactors::Own(Box::new(Redactor::new(Some(keys)))),
        });
        let transport = dsn.map(|d| Arc::new(Transport::new(d, &opts)));
        let sessions = transport.as_ref().filter(|_| opts.sessions_on()).map(|t| {
            let release = opts.release.clone().unwrap_or_default();
            let environment = opts.environment.clone().unwrap_or_default();
            Aggregates::start(Arc::clone(t), release, environment, opts.session_interval)
        });
        Ok(Client {
            budget: Budget::new(opts.error_budget.clone()),
            opts,
            transport,
            redactor,
            sessions,
        })
    }

    /// The client's options, defaults filled in.
    pub fn options(&self) -> &Options {
        &self.opts
    }

    /// Whether it sends: it has a DSN.
    pub fn is_enabled(&self) -> bool {
        self.transport.is_some()
    }

    pub(crate) fn log(&self, message: impl FnOnce() -> String) {
        if let Some(t) = &self.transport {
            t.log(message);
        }
    }

    /// Sends an event; its id, or `None` when it was not sent.
    pub(crate) fn capture(&self, mut e: Event) -> Option<String> {
        let transport = self.transport.as_ref()?;
        let Some(suppressed) = self.budget.allow(issue_of(&e), Instant::now()) else {
            self.log(|| "dropped an event: over the error budget".into());
            return None;
        };
        if self.opts.sample_rate < 1.0 && random_fraction() >= self.opts.sample_rate {
            return None;
        }
        e.suppressed = suppressed;
        if e.event_id.is_empty() {
            e.event_id = new_id(16);
        }
        e.timestamp.get_or_insert_with(SystemTime::now);
        e.level.get_or_insert(if e.exceptions.is_empty() {
            Level::Info
        } else {
            Level::Error
        });
        if let Some(user) = &mut e.user {
            if !self.opts.send_default_pii {
                user.ip_address = None;
            } else if user.ip_address.is_none() {
                user.ip_address = e.request.as_ref().and_then(|r| r.client_address.clone());
            }
        }
        if let Some(before) = &self.opts.before_send {
            e = before(e)?;
        }
        let id = e.event_id.clone();
        let mut body = serde_json::to_vec(&self.logs_export(self.event_record(&e))).ok()?;
        if body.len() > MAX_EVENT_BYTES {
            // Fixwire refuses it: it goes without its breadcrumbs, details and source lines, or
            // not at all.
            e.breadcrumbs.clear();
            e.extra.clear();
            for f in e.exceptions.iter_mut().flat_map(|x| x.frames.iter_mut()) {
                f.context_line = None;
                f.pre_context.clear();
                f.post_context.clear();
            }
            body = serde_json::to_vec(&self.logs_export(self.event_record(&e))).ok()?;
            if body.len() > MAX_EVENT_BYTES {
                self.log(|| "dropped an event: larger than 1 MB".into());
                return None;
            }
        }
        transport
            .send(Request::json("/v1/logs", Category::Error, body))
            .then_some(id)
    }

    /// Queues a Fixwire JSON request.
    pub(crate) fn send_json(&self, path: &str, category: Category, body: &Value) -> bool {
        let Some(transport) = &self.transport else {
            return false;
        };
        match serde_json::to_vec(body) {
            Ok(body) => transport.send(Request::json(path, category, body)),
            Err(_) => false,
        }
    }

    /// Masks secrets and personal data in `m`, but for the keys in `skip`.
    pub(crate) fn scrub(&self, mut m: Map<String, Value>, skip: &[&str]) -> Map<String, Value> {
        let Some(redactor) = &self.redactor else {
            return m;
        };
        let kept: Vec<(String, Value)> = skip.iter().filter_map(|k| m.remove_entry(*k)).collect();
        let (masked, _) = redactor.walk(&Value::Object(m));
        let mut out = match masked {
            Value::Object(o) => o,
            _ => Map::new(),
        };
        out.extend(kept);
        out
    }

    /// Masks secrets and personal data in `s`.
    pub(crate) fn mask(&self, s: &str) -> String {
        match &self.redactor {
            Some(r) if !s.is_empty() => r.mask(s).0.into_owned(),
            _ => s.to_owned(),
        }
    }

    /// Whether trace headers may go to `url`: it holds one of the
    /// `trace_propagation_targets`.
    pub fn should_propagate(&self, url: &str) -> bool {
        self.opts
            .trace_propagation_targets
            .iter()
            .any(|t| !t.is_empty() && url.contains(t.as_str()))
    }

    /// Waits until what was captured is sent, or `timeout`; false when time
    /// ran out.
    pub fn flush(&self, timeout: Duration) -> bool {
        if let Some(s) = &self.sessions {
            s.send();
        }
        self.transport.as_ref().is_none_or(|t| t.flush(timeout))
    }

    /// Flushes and stops the client, within `timeout`.
    pub fn close(&self, timeout: Duration) {
        let start = Instant::now();
        if let Some(s) = &self.sessions {
            s.stop();
        }
        self.flush(timeout);
        if let Some(t) = &self.transport {
            t.close(timeout.saturating_sub(start.elapsed()));
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(s) = &self.sessions {
            s.stop();
        }
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("options", &self.opts)
            .field("enabled", &self.is_enabled())
            .finish_non_exhaustive()
    }
}

/// The redactor with the default keys is the process's; other keys get
/// their own.
enum Redactors {
    Shared(&'static Redactor),
    Own(Box<Redactor>),
}

impl std::ops::Deref for Redactors {
    type Target = Redactor;

    fn deref(&self) -> &Redactor {
        match self {
            Redactors::Shared(r) => r,
            Redactors::Own(r) => r,
        }
    }
}

/// `n` random bytes in hex: 16 for event and trace ids, 8 for span ids.
pub(crate) fn new_id(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    if getrandom::fill(&mut bytes).is_err() {
        // No randomness from the system: unique enough from the clock.
        let t = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (t >> ((i % 16) * 8)) as u8 ^ i as u8;
        }
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A random number in [0, 1).
pub(crate) fn random_fraction() -> f64 {
    let mut b = [0u8; 8];
    let _ = getrandom::fill(&mut b);
    (u64::from_le_bytes(b) >> 11) as f64 / (1u64 << 53) as f64
}
