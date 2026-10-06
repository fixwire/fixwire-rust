//! The client: what is captured for one project goes through it, redacted
//! and budgeted, to the transport.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Map, Value};

use crate::budget::{Budget, issue_of};
use crate::dsn::{Dsn, InvalidDsn};
use crate::http::ComparedUrl;
use crate::hub::{MAX_CHAIN, guarded};
use crate::limits;
use crate::options::Options;
use crate::otlp::Export;
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
        Ok(Client::with_dsn(opts, dsn))
    }

    /// A client for `opts`, or a disabled one when the DSN (given or in
    /// `FIXWIRE_DSN`) is invalid, which is said on stderr: what `init` sets
    /// up, so a typo can't stop the app.
    pub(crate) fn new_or_off(opts: Options) -> Client {
        let mut opts = opts.with_defaults();
        match opts.dsn.as_deref().map(str::parse).transpose() {
            Ok(dsn) => Client::with_dsn(opts, dsn),
            Err(e) => {
                eprintln!("{e}: the SDK is off");
                opts.dsn = None;
                Client::with_dsn(opts, None)
            }
        }
    }

    fn with_dsn(opts: Options, dsn: Option<Dsn>) -> Client {
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
        Client {
            budget: Budget::new(opts.error_budget.clone()),
            opts,
            transport,
            redactor,
            sessions,
        }
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
            // A callback that panics is skipped: the event goes as it was.
            match guarded(|| before(e.clone())) {
                Some(changed) => e = changed?,
                None => self.log(|| "before_send panicked: the event goes as it was".into()),
            }
        }
        // A chain of at most 10, each with the newest `max_stack_frames`.
        e.exceptions.truncate(MAX_CHAIN);
        for x in &mut e.exceptions {
            let older = x.frames.len().saturating_sub(self.opts.max_stack_frames);
            x.frames.drain(..older);
        }
        let id = e.event_id.clone();
        let mut record = serde_json::to_vec(&self.event_record(&e)).ok()?;
        // Fixwire refuses more: it goes without its breadcrumbs, then without its contexts (Rust
        // frames have no local variables to leave out between), or not at all.
        if record.len() > MAX_EVENT_BYTES && !e.breadcrumbs.is_empty() {
            e.breadcrumbs.clear();
            record = serde_json::to_vec(&self.event_record(&e)).ok()?;
        }
        if record.len() > MAX_EVENT_BYTES && !e.contexts.is_empty() {
            e.contexts.clear();
            record = serde_json::to_vec(&self.event_record(&e)).ok()?;
        }
        if record.len() > MAX_EVENT_BYTES {
            self.log(|| "dropped an event: larger than 1 MB".into());
            return None;
        }
        let body = self.export(Export::Logs, &[record]);
        transport
            .send(Request::json("/v1/logs", Category::Error, body))
            .then_some(id)
    }

    /// Queues a request.
    pub(crate) fn send(&self, request: Request) -> bool {
        self.transport.as_ref().is_some_and(|t| t.send(request))
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

    /// Masks secrets and personal data in `m`, but for the keys in `skip`,
    /// and cuts its strings to `max_value_length`.
    pub(crate) fn scrub(&self, mut m: Map<String, Value>, skip: &[&str]) -> Map<String, Value> {
        let kept: Vec<(String, Value)> = skip.iter().filter_map(|k| m.remove_entry(*k)).collect();
        let limit = self.opts.max_value_length;
        let out = match &self.redactor {
            Some(redactor) => redactor.walk_within(&Value::Object(m), limit).0,
            None => limits::cut_strings(Value::Object(m), limit),
        };
        let mut out = match out {
            Value::Object(o) => o,
            _ => Map::new(),
        };
        out.extend(kept);
        out
    }

    /// `s` as it is sent: secrets and personal data masked, then cut to
    /// `max_value_length`.
    pub(crate) fn clean(&self, s: &str) -> String {
        let limit = self.opts.max_value_length;
        match &self.redactor {
            Some(r) => r.mask_within(s, limit).0.into_owned(),
            None => limits::cut(s, limit, false).into_owned(),
        }
    }

    /// Whether trace headers may go to `url`: it matches one of the
    /// `trace_propagation_targets` (see `Options`).
    pub fn should_propagate(&self, url: &str) -> bool {
        let url = ComparedUrl::of(url);
        self.opts
            .trace_propagation_targets
            .iter()
            .any(|t| url.matches(t))
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
