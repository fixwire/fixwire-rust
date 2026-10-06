//! Outgoing HTTP requests, for any client: a client span in the current
//! trace, the trace headers for `trace_propagation_targets`, and an `http`
//! breadcrumb.

use serde_json::{Map, Value};

use crate::hub::Hub;
use crate::span::Span;
use crate::types::{Breadcrumb, Level};

/// An outgoing HTTP request, timed and traced:
///
/// ```no_run
/// let call = fixwire::OutgoingRequest::start("POST", "http://inventory.internal/reservations");
/// // add call.headers() to the request, send it, then:
/// call.finish(Some(201), None);
/// ```
#[derive(Debug)]
pub struct OutgoingRequest {
    hub: Hub,
    method: String,
    url: String,
    span: Option<Span>,
    headers: Vec<(&'static str, String)>,
}

impl OutgoingRequest {
    /// Starts the request's span under the current scope's span (when the
    /// trace is kept), and works out the trace headers it may carry.
    pub fn start(method: &str, url: &str) -> OutgoingRequest {
        let hub = Hub::current();
        let plain = url.split(['?', '#']).next().unwrap_or(url).to_owned();
        let parent = hub.configure_scope(|s| s.span.clone());
        let span = parent.as_ref().filter(|p| p.sampled()).map(|p| {
            let span = p.start_child(format!("{method} {plain}"), "http.client");
            span.set_attribute("http.request.method", method);
            span.set_attribute("url.full", plain.clone());
            if let Some(host) = host_of(&plain) {
                span.set_attribute("server.address", host);
            }
            span
        });
        let mut headers = Vec::new();
        let propagate = hub.client().is_some_and(|c| c.should_propagate(url));
        if let Some(from) = span.as_ref().or(parent.as_ref()).filter(|_| propagate) {
            headers.push(("traceparent", from.traceparent()));
            if let Some(state) = from.tracestate() {
                headers.push(("tracestate", state.to_owned()));
            }
            if let Some(baggage) = from.baggage() {
                headers.push(("baggage", baggage.to_owned()));
            }
        }
        OutgoingRequest {
            hub,
            method: method.to_owned(),
            url: plain,
            span,
            headers,
        }
    }

    /// The headers to add to the request: `traceparent`, and `tracestate`
    /// and `baggage` when the trace has them; none for hosts outside
    /// `trace_propagation_targets`.
    pub fn headers(&self) -> &[(&'static str, String)] {
        &self.headers
    }

    /// Ends the request: its status, or the error that stopped it.
    pub fn finish(self, status: Option<u16>, error: Option<&dyn std::error::Error>) {
        if let Some(span) = &self.span {
            if let Some(s) = status {
                span.set_attribute("http.response.status_code", s);
            }
            if let Some(e) = error {
                span.set_error(e.to_string());
            } else if status.is_some_and(|s| s >= 400) {
                span.set_error(status.map(|s| s.to_string()).unwrap_or_default());
            }
            span.finish();
        }
        let mut data = Map::new();
        data.insert("method".into(), Value::from(self.method));
        data.insert("url".into(), Value::from(self.url));
        if let Some(s) = status {
            data.insert("status_code".into(), s.into());
        }
        let failed = error.is_some() || status.is_some_and(|s| s >= 500);
        self.hub.add_breadcrumb(Breadcrumb {
            ty: Some("http".into()),
            category: Some("http".into()),
            level: Some(if failed { Level::Error } else { Level::Info }),
            data,
            ..Breadcrumb::default()
        });
    }
}

fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split('/').next()?;
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    Some(host.split(':').next().unwrap_or(host).to_owned()).filter(|h| !h.is_empty())
}
