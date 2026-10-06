//! A `tracing-subscriber` layer: events become breadcrumbs (info and up, by
//! default), error events become Fixwire events. An event's `error` field
//! (`tracing::error!(error = &e as &dyn Error, "…")`) is sent as the
//! exception, with its chain of sources and the stack where it was logged.
//!
//! ```ignore
//! use tracing_subscriber::prelude::*;
//! tracing_subscriber::registry().with(fixwire::tracing::layer()).init();
//! ```

use std::error::Error;
use std::fmt;

use serde_json::{Map, Value};
use tracing_subscriber::layer::Context;

use crate::hub::{Hub, exceptions_of};
use crate::stacktrace;
use crate::types::{Breadcrumb, Event, Level, Mechanism};

/// The layer, sending events at `event_level` and up and keeping
/// breadcrumbs from `breadcrumb_level`.
#[derive(Clone, Debug)]
pub struct FixwireLayer {
    event_level: Level,
    breadcrumb_level: Level,
}

/// The layer with its defaults: error events are sent, info and up are
/// breadcrumbs.
pub fn layer() -> FixwireLayer {
    FixwireLayer {
        event_level: Level::Error,
        breadcrumb_level: Level::Info,
    }
}

impl FixwireLayer {
    /// Events at this level and up are sent (default error).
    pub fn event_level(mut self, level: Level) -> FixwireLayer {
        self.event_level = level;
        self
    }

    /// Events at this level and up become breadcrumbs (default info).
    pub fn breadcrumb_level(mut self, level: Level) -> FixwireLayer {
        self.breadcrumb_level = level;
        self
    }
}

/// Loggers whose error records repeat what the SDK reported already: tower-http's panic layer
/// logs each panic it answers 500 to.
const REPORTED_ELSEWHERE: [&str; 1] = ["tower_http::catch_panic"];

fn level_of(level: ::tracing::Level) -> Level {
    match level {
        ::tracing::Level::ERROR => Level::Error,
        ::tracing::Level::WARN => Level::Warning,
        ::tracing::Level::INFO => Level::Info,
        _ => Level::Debug,
    }
}

impl<S: ::tracing::Subscriber> tracing_subscriber::Layer<S> for FixwireLayer {
    fn on_event(&self, event: &::tracing::Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        // The SDK's own records never come back to it, and what a library logs about a panic it
        // caught isn't sent again: the panic hook reported the panic as a crash.
        if meta.target().starts_with("fixwire")
            || REPORTED_ELSEWHERE
                .iter()
                .any(|t| meta.target().starts_with(t))
        {
            return;
        }
        let level = level_of(*meta.level());
        if level < self.breadcrumb_level && level < self.event_level {
            return;
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        let hub = Hub::current();
        let Some(client) = hub.client().filter(|c| c.is_enabled()) else {
            return;
        };
        if level >= self.event_level {
            let mut e = Event {
                level: Some(level),
                message: fields.message.clone(),
                ..Event::default()
            };
            if let Some(chain) = fields.error {
                let frames = stacktrace::capture_skipping(
                    client.options(),
                    &["tracing", "tracing_core", "tracing_subscriber"],
                );
                let mut exceptions = chain;
                if let Some(outer) = exceptions.first_mut() {
                    outer.frames = frames;
                    outer.mechanism = Mechanism {
                        ty: "tracing".into(),
                        handled: true,
                    };
                }
                e.exceptions = exceptions;
            }
            e.extra = fields.values;
            e.extra.insert("logger".into(), meta.target().into());
            hub.capture_event(e);
        } else {
            hub.add_breadcrumb(Breadcrumb {
                ty: Some("log".into()),
                category: Some(meta.target().to_owned()),
                message: fields.message,
                level: Some(level),
                data: fields.values,
                ..Breadcrumb::default()
            });
        }
    }
}

/// An event's fields: its message, an `error`, and the rest.
#[derive(Default)]
struct Fields {
    message: Option<String>,
    error: Option<Vec<crate::types::Exception>>,
    values: Map<String, Value>,
}

impl ::tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &::tracing::field::Field, value: &dyn fmt::Debug) {
        let text = format!("{value:?}");
        if field.name() == "message" {
            self.message = Some(text);
        } else {
            self.values.insert(field.name().into(), text.into());
        }
    }

    fn record_str(&mut self, field: &::tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_owned());
        } else {
            self.values.insert(field.name().into(), value.into());
        }
    }

    fn record_i64(&mut self, field: &::tracing::field::Field, value: i64) {
        self.values.insert(field.name().into(), value.into());
    }

    fn record_u64(&mut self, field: &::tracing::field::Field, value: u64) {
        self.values.insert(field.name().into(), value.into());
    }

    fn record_f64(&mut self, field: &::tracing::field::Field, value: f64) {
        self.values.insert(field.name().into(), value.into());
    }

    fn record_bool(&mut self, field: &::tracing::field::Field, value: bool) {
        self.values.insert(field.name().into(), value.into());
    }

    fn record_error(&mut self, field: &::tracing::field::Field, value: &(dyn Error + 'static)) {
        if self.error.is_none() {
            self.error = Some(exceptions_of(value, Vec::new(), Mechanism::default()));
        }
        self.values
            .insert(field.name().into(), value.to_string().into());
    }
}
