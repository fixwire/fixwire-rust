//! The Fixwire SDK for Rust: errors and panics, traces, release health,
//! cron monitors and feedback.
//!
//! ```no_run
//! // FIXWIRE_DSN in the environment; without it the SDK does nothing.
//! let _fixwire = fixwire::init(fixwire::Options {
//!     release: Some("api@1.4.0".into()),
//!     ..Default::default()
//! }); // keep the guard: dropping it sends what is left
//!
//! if let Err(e) = "4x".parse::<u32>() {
//!     fixwire::capture_error(&e);
//! }
//! ```
//!
//! Panics are reported (the panic hook that was there still runs), secrets
//! and personal data are masked on the device with the same rules as the
//! Fixwire server, and a crash loop costs a few events and a count, not your
//! quota. Data travels as OpenTelemetry (OTLP/HTTP JSON) to the endpoints of
//! Fixwire protocol v1.
//!
//! Features: `tower` (a layer for axum, hyper and tonic servers: a scope, a
//! server span and a session per request), `axum` (requests named after
//! their matched route), `tracing` (a `tracing-subscriber` layer: events as
//! breadcrumbs, error events as Fixwire events).

#![warn(missing_docs)]

mod budget;
mod checkin;
mod client;
mod dsn;
mod http;
mod hub;
mod options;
mod otlp;
mod panic;
mod redaction;
mod scope;
mod sessions;
mod span;
mod stacktrace;
mod transport;
mod types;

#[cfg(feature = "tower")]
pub mod tower;
#[cfg(feature = "tracing")]
pub mod tracing;

use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value};

pub use checkin::{CheckIn, CheckInStatus, Feedback, MonitorConfig, MonitorSchedule, with_monitor};
pub use client::Client;
pub use dsn::{Dsn, InvalidDsn};
pub use http::OutgoingRequest;
pub use hub::{FutureExt, Hub, HubBound};
pub use options::{BeforeBreadcrumb, BeforeSend, ErrorBudget, Options};
pub use scope::Scope;
pub use sessions::RequestSessionGuard;
pub use span::{Span, SpanKind, start_span, trace, trace_future};
pub use types::{Breadcrumb, Event, Exception, Frame, Level, Mechanism, Request, User};

/// The README's examples, compiled and run as doctests so they stay right.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeExamples;

/// The SDK's name, as OpenTelemetry's `telemetry.sdk.name` and Fixwire JSON
/// bodies say it.
pub const SDK_NAME: &str = "fixwire.rust";
/// The SDK's version.
pub const SDK_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The SDK in Fixwire JSON bodies.
pub(crate) fn sdk() -> Value {
    let mut m = Map::new();
    m.insert("name".into(), SDK_NAME.into());
    m.insert("version".into(), SDK_VERSION.into());
    Value::Object(m)
}

/// Sets the SDK up: the process's hub gets a client for `opts`, and panics
/// are reported. Keep the guard while the program runs: dropping it sends
/// what is left (up to two seconds) and stops the SDK. Without a DSN the SDK
/// stays off, and so does it with an invalid one (which it says on stderr).
#[must_use = "dropping the guard stops the SDK: keep it until the program ends"]
pub fn init(opts: Options) -> ClientInitGuard {
    let client = match Client::new(opts) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("{e}");
            Arc::new(
                Client::new(Options {
                    dsn: None,
                    ..Options::default()
                })
                .expect("no DSN is valid"),
            )
        }
    };
    if client.is_enabled() && client.options().capture_panics {
        panic::install();
    }
    Hub::main().bind_client(Some(Arc::clone(&client)));
    ClientInitGuard { client }
}

/// Sends what is left and stops the SDK when dropped; see `init`.
#[derive(Debug)]
pub struct ClientInitGuard {
    client: Arc<Client>,
}

impl ClientInitGuard {
    /// The client `init` set up.
    pub fn client(&self) -> &Arc<Client> {
        &self.client
    }
}

impl Drop for ClientInitGuard {
    fn drop(&mut self) {
        self.client.close(Duration::from_secs(2));
    }
}

/// Sends an error with the stack where it was captured and its chain of
/// sources; its event id, or `None` when it was not sent.
pub fn capture_error<E: Error + ?Sized>(err: &E) -> Option<String> {
    Hub::current().capture_error(err)
}

/// Sends a message.
pub fn capture_message(message: impl Into<String>, level: Level) -> Option<String> {
    Hub::current().capture_message(message, level)
}

/// Sends an event as it is, with what the scope knows.
pub fn capture_event(event: Event) -> Option<String> {
    Hub::current().capture_event(event)
}

/// Reports a run of a scheduled job (see `with_monitor`).
pub fn capture_check_in(check_in: CheckIn) -> Option<String> {
    Hub::current().client()?.capture_check_in(check_in)
}

/// Sends what someone said about an error or an AI answer.
pub fn capture_feedback(feedback: Feedback) -> Option<String> {
    Hub::current().capture_feedback(feedback)
}

/// Records something that happened.
pub fn add_breadcrumb(breadcrumb: Breadcrumb) {
    Hub::current().add_breadcrumb(breadcrumb);
}

/// Changes the current scope.
pub fn configure_scope<R>(f: impl FnOnce(&mut Scope) -> R) -> R {
    Hub::current().configure_scope(f)
}

/// Runs `f` with a copy of the current scope, set up by `configure`.
pub fn with_scope<R>(configure: impl FnOnce(&mut Scope), f: impl FnOnce() -> R) -> R {
    Hub::current().with_scope(configure, f)
}

/// Sets who the work is for.
pub fn set_user(user: Option<User>) {
    configure_scope(|s| s.set_user(user));
}

/// Sets a searchable tag.
pub fn set_tag(key: impl Into<String>, value: impl Into<String>) {
    configure_scope(|s| s.set_tag(key, value));
}

/// Waits until what was captured is sent, or `timeout`; false when time
/// ran out. Call it before a short-lived program exits (the guard does).
pub fn flush(timeout: Duration) -> bool {
    Hub::current().flush(timeout)
}
