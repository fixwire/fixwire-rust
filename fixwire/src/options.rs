//! How the SDK is set up.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::limits;
use crate::types::{Breadcrumb, Event};

/// Changes an event before it is sent, or drops it (`None`).
pub type BeforeSend = Arc<dyn Fn(Event) -> Option<Event> + Send + Sync>;
/// Changes a breadcrumb before it is kept, or drops it (`None`).
pub type BeforeBreadcrumb = Arc<dyn Fn(Breadcrumb) -> Option<Breadcrumb> + Send + Sync>;

/// The SDK's options. Only `dsn` is needed; without one (and without
/// `FIXWIRE_DSN`) the SDK does nothing.
///
/// ```
/// let _guard = fixwire::init(fixwire::Options {
///     dsn: Some("https://fw_pk_live_…@ingest.eu.fixwire.io".into()),
///     release: Some("api@1.4.0".into()),
///     traces_sample_rate: 0.2,
///     ..Default::default()
/// });
/// ```
#[derive(Clone)]
pub struct Options {
    /// The project's DSN; `FIXWIRE_DSN` when `None`.
    pub dsn: Option<String>,
    /// The app's version, such as `api@1.4.0` or a commit SHA;
    /// `FIXWIRE_RELEASE` when `None`.
    pub release: Option<String>,
    /// Where it runs: `production` (the default, `FIXWIRE_ENVIRONMENT`),
    /// `staging`, …
    pub environment: Option<String>,
    /// The machine's name; the host name when `None`.
    pub server_name: Option<String>,
    /// The service's name: `OTEL_SERVICE_NAME` when `None`, else the name in
    /// a `name@version` release.
    pub service_name: Option<String>,

    /// The share of errors and messages sent (default 1).
    pub sample_rate: f64,
    /// The share of new traces kept (default 0: no tracing). Traces
    /// continued from a caller follow its decision.
    pub traces_sample_rate: f64,
    /// The URLs outgoing requests carry trace headers to (default none, so
    /// no other service sees them). A URL is compared without its user info,
    /// query and fragment: a target with `://` matches URLs that start with
    /// it (`https://api.example.com/v2`); one starting with `/` matches
    /// relative URLs whose path starts with it; any other is a host, with a
    /// port if it has one, and matches that host and its subdomains
    /// (`example.com` matches `api.example.com`, not `badexample.com`).
    pub trace_propagation_targets: Vec<String>,

    /// Bounds the events sent per issue and per minute, so a crash loop
    /// costs a few events and a count.
    pub error_budget: ErrorBudget,

    /// May change an event, or drop it.
    pub before_send: Option<BeforeSend>,
    /// May change a breadcrumb, or drop it.
    pub before_breadcrumb: Option<BeforeBreadcrumb>,
    /// The breadcrumbs kept per scope (default 100).
    pub max_breadcrumbs: usize,
    /// Sends the user's IP address and request headers that may identify
    /// them (off by default).
    pub send_default_pii: bool,
    /// Masks secrets and personal data on the device, with the same rules as
    /// the server (on by default).
    pub redact: bool,
    /// Replace the default key fragments (password, token, cookie, …) whose
    /// values are filtered whole.
    pub sensitive_keys: Option<Vec<String>>,
    /// The longest string sent, in bytes of UTF-8 (default 1024): a longer
    /// one is cut on a character boundary and ends in `...`, within the
    /// limit. Redaction runs first, over the part kept and the next 16 kB.
    pub max_value_length: usize,
    /// The frames sent per exception (default 100): the newest are kept.
    pub max_stack_frames: usize,
    /// How many source lines above and below each of the app's frames are
    /// read when the file is there (default and most 5; 0 turns it off).
    pub context_lines: usize,
    /// The project's root: frames under it are the app's, named relative to
    /// it. The working directory when `None`.
    pub project_root: Option<PathBuf>,
    /// Module path prefixes whose frames are the app's (`my_crate::`); the
    /// binary's own crate is already.
    pub in_app_include: Vec<String>,
    /// Module path prefixes whose frames are not the app's.
    pub in_app_exclude: Vec<String>,

    /// Reports panics (on by default): the panic hook that was there still
    /// runs.
    pub capture_panics: bool,
    /// The requests waiting to be sent (default 100).
    pub max_queue: usize,
    /// How long a request to Fixwire may take (default 30 s).
    pub timeout: Duration,
    /// Logs what the SDK does to stderr (`FIXWIRE_DEBUG`).
    pub debug: bool,
    /// Counts each request a server serves for release health, sent about
    /// every `session_interval` (on when there is a release).
    pub auto_session_tracking: bool,
    /// How often request sessions are sent (default a minute).
    pub session_interval: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            dsn: None,
            release: None,
            environment: None,
            server_name: None,
            service_name: None,
            sample_rate: 1.0,
            traces_sample_rate: 0.0,
            trace_propagation_targets: Vec::new(),
            error_budget: ErrorBudget::default(),
            before_send: None,
            before_breadcrumb: None,
            max_breadcrumbs: 100,
            send_default_pii: false,
            redact: true,
            sensitive_keys: None,
            max_value_length: 1024,
            max_stack_frames: 100,
            context_lines: 5,
            project_root: None,
            in_app_include: Vec::new(),
            in_app_exclude: Vec::new(),
            capture_panics: true,
            max_queue: 100,
            timeout: Duration::from_secs(30),
            debug: false,
            auto_session_tracking: true,
            session_interval: Duration::from_secs(60),
        }
    }
}

impl fmt::Debug for Options {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Options")
            .field("dsn", &self.dsn.as_ref().map(|_| "…"))
            .field("release", &self.release)
            .field("environment", &self.environment)
            .field("service_name", &self.service_name)
            .field("sample_rate", &self.sample_rate)
            .field("traces_sample_rate", &self.traces_sample_rate)
            .field("redact", &self.redact)
            .field("debug", &self.debug)
            .finish_non_exhaustive()
    }
}

/// Bounds the errors and messages sent: each issue (a cheap fingerprint of
/// the event) may send a burst, then so many a minute, within a budget for
/// all of them. Occurrences held back are counted and ride on the issue's
/// next event, so issue counts stay right.
#[derive(Clone, Debug)]
pub struct ErrorBudget {
    /// Events of one issue sent at once (default 10).
    pub per_issue_burst: u32,
    /// Then this many a minute (default 1).
    pub per_issue_per_minute: f64,
    /// Events a minute across issues (default 600).
    pub per_minute: f64,
    /// Sends every event.
    pub disabled: bool,
}

impl Default for ErrorBudget {
    fn default() -> Self {
        ErrorBudget {
            per_issue_burst: 10,
            per_issue_per_minute: 1.0,
            per_minute: 600.0,
            disabled: false,
        }
    }
}

impl Options {
    /// Fills what isn't set from the environment, and bounds the rest.
    pub(crate) fn with_defaults(mut self) -> Options {
        let env = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        self.dsn = self
            .dsn
            .filter(|d| !d.trim().is_empty())
            .or_else(|| env("FIXWIRE_DSN"));
        self.release = self
            .release
            .filter(|r| !r.is_empty())
            .or_else(|| env("FIXWIRE_RELEASE"));
        self.environment = Some(
            self.environment
                .filter(|e| !e.is_empty())
                .or_else(|| env("FIXWIRE_ENVIRONMENT"))
                .unwrap_or_else(|| "production".into()),
        );
        if self.server_name.is_none() {
            self.server_name = host_name();
        }
        if self.service_name.is_none() {
            self.service_name = env("OTEL_SERVICE_NAME").or_else(|| {
                // "api" of "api@1.4.0"
                self.release
                    .as_deref()
                    .and_then(|r| r.split_once('@'))
                    .map(|(name, _)| name.to_owned())
            });
        }
        if !(self.sample_rate > 0.0 && self.sample_rate <= 1.0) {
            self.sample_rate = 1.0;
        }
        if !(0.0..=1.0).contains(&self.traces_sample_rate) {
            self.traces_sample_rate = 0.0;
        }
        if self.max_queue == 0 {
            self.max_queue = 100;
        }
        if self.max_value_length == 0 {
            self.max_value_length = 1024;
        }
        // Room for the "..." that ends a cut string.
        self.max_value_length = self.max_value_length.max(3);
        // The app's own configuration is cut like any string, but not redacted: masking
        // `api@1.2.3.example` as an email would break release health.
        let limit = self.max_value_length;
        for s in [
            &mut self.release,
            &mut self.environment,
            &mut self.server_name,
            &mut self.service_name,
        ]
        .into_iter()
        .flatten()
        {
            if s.len() > limit {
                *s = limits::cut(s, limit, false).into_owned();
            }
        }
        if self.max_stack_frames == 0 {
            self.max_stack_frames = 100;
        }
        if self.session_interval.is_zero() {
            self.session_interval = Duration::from_secs(60);
        }
        if !self.debug {
            self.debug = env("FIXWIRE_DEBUG")
                .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"));
        }
        if self.project_root.is_none() {
            self.project_root = std::env::current_dir().ok();
        }
        self
    }

    pub(crate) fn sessions_on(&self) -> bool {
        self.release.is_some() && self.auto_session_tracking
    }
}

/// The host's name, from the environment or the system's files.
fn host_name() -> Option<String> {
    ["HOSTNAME", "COMPUTERNAME"]
        .iter()
        .find_map(|v| std::env::var(v).ok())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
}
