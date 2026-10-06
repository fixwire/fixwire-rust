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
        let parent = hub.try_configure_scope(|s| s.span.clone()).flatten();
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
                // The error's `Display` is the app's code.
                let message = crate::hub::guarded(|| e.to_string());
                span.set_error(message.unwrap_or_else(|| "[Unreadable]".into()));
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

/// A URL as `trace_propagation_targets` compare it: without its user info,
/// query and fragment, its scheme and host in lower case; and its host and
/// port (the scheme's when it names none), when it is absolute.
pub(crate) struct ComparedUrl {
    url: String,
    host: Option<(String, Option<u16>)>,
}

impl ComparedUrl {
    pub(crate) fn of(url: &str) -> ComparedUrl {
        // As URL parsers do: tabs and line breaks are left out, the query and fragment end it.
        let url: String = url
            .trim()
            .chars()
            .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
            .collect();
        let url = &url[..url.find(['?', '#']).unwrap_or(url.len())];
        let scheme_ok = |s: &str| {
            s.starts_with(|c: char| c.is_ascii_alphabetic())
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        };
        let Some((scheme, rest)) = url.split_once("://").filter(|(s, _)| scheme_ok(s)) else {
            return ComparedUrl {
                url: url.to_owned(),
                host: None,
            };
        };
        // The authority ends where the path starts: "/", or "\" as browsers read it.
        let (authority, path) = rest.split_at(rest.find(['/', '\\']).unwrap_or(rest.len()));
        let host_port = authority
            .rsplit_once('@')
            .map_or(authority, |(_, h)| h)
            .to_ascii_lowercase();
        let scheme = scheme.to_ascii_lowercase();
        let (host, port) = split_port(&host_port);
        let port = match port {
            Some(p) => p.parse().ok(),
            None => match scheme.as_str() {
                "http" | "ws" => Some(80),
                "https" | "wss" => Some(443),
                _ => None,
            },
        };
        ComparedUrl {
            host: Some((host.to_owned(), port)),
            url: format!("{scheme}://{host_port}{path}"),
        }
    }

    /// Whether the URL matches a target: one with `://` is a prefix of the
    /// URL; one starting with `/`, of a relative URL's path; any other is a
    /// host (with a port if it has one) that the URL's host is, or is a
    /// subdomain of.
    pub(crate) fn matches(&self, target: &str) -> bool {
        if target.is_empty() {
            return false;
        }
        if target.contains("://") {
            return self.host.is_some() && self.url.starts_with(target);
        }
        if target.starts_with('/') {
            return self.host.is_none()
                && !self.url.starts_with("//")
                && self.url.starts_with(target);
        }
        let Some((host, port)) = &self.host else {
            return false;
        };
        let target = target.to_ascii_lowercase();
        let (want, want_port) = split_port(&target);
        let port_ok = match want_port {
            None => true,
            Some(p) => p.parse::<u16>().ok().is_some_and(|p| Some(p) == *port),
        };
        let host_ok = !want.is_empty()
            && (host == want
                || host
                    .strip_suffix(want)
                    .is_some_and(|sub| sub.ends_with('.')));
        host_ok && port_ok
    }
}

/// `host:port` as the host and the port, if there is one (`[::1]:8080` too).
fn split_port(host_port: &str) -> (&str, Option<&str>) {
    match host_port.rsplit_once(':') {
        Some((host, port)) if !port.contains(']') => (host, Some(port)),
        _ => (host_port, None),
    }
}

#[cfg(test)]
mod tests {
    use super::ComparedUrl;

    fn matches(url: &str, target: &str) -> bool {
        ComparedUrl::of(url).matches(target)
    }

    #[test]
    fn targets_with_a_scheme_are_prefixes_of_the_url_without_user_query_and_fragment() {
        let t = "https://api.example.com/v2";
        assert!(matches("https://api.example.com/v2/orders?x=1", t));
        assert!(matches("HTTPS://API.Example.COM/v2/orders", t));
        assert!(matches("https://ada:secret@api.example.com/v2/x", t));
        assert!(!matches("https://api.example.com/v1/orders", t));
        assert!(!matches(
            "https://evil.net/?u=https://api.example.com/v2",
            t
        ));
        assert!(!matches("https://evil.net/#https://api.example.com/v2", t));
        assert!(!matches("/v2/orders", t));
    }

    #[test]
    fn hosts_match_themselves_and_their_subdomains_only() {
        for url in [
            "https://example.com/x",
            "https://api.example.com/x",
            "http://a.b.example.com:8080/x",
            "https://API.EXAMPLE.COM",
            "https://user:pass@api.example.com/x",
        ] {
            assert!(matches(url, "example.com"), "{url}");
            assert!(matches(url, "Example.COM"), "{url}");
        }
        for url in [
            "https://badexample.com/x",
            "https://example.com.evil.net/x",
            "https://evil.net/example.com",
            "https://evil.net/?h=example.com",
            "https://example.com@evil.net/x",
            "https://evil.net\\@example.com/x",
            "https://evil.net#@example.com",
            "/example.com",
            "example.com",
        ] {
            assert!(!matches(url, "example.com"), "{url}");
        }
    }

    #[test]
    fn hosts_with_a_port_match_that_port() {
        assert!(matches(
            "http://inventory.internal:8080/r",
            "inventory.internal:8080"
        ));
        assert!(matches("https://api.example.com/x", "example.com:443"));
        assert!(matches("http://api.example.com/x", "example.com:80"));
        assert!(!matches(
            "http://inventory.internal:9090/r",
            "inventory.internal:8080"
        ));
        assert!(!matches("https://api.example.com/x", "example.com:8443"));
        assert!(!matches("https://api.example.com/x", "example.com:https"));
        assert!(matches("http://[::1]:8080/x", "[::1]:8080"));
        assert!(matches("http://[::1]/x", "[::1]"));
    }

    #[test]
    fn paths_match_relative_urls_only() {
        assert!(matches("/api/orders?x=1", "/api"));
        assert!(!matches("/other", "/api"));
        assert!(!matches("https://example.com/api", "/api"));
        assert!(!matches("//evil.net/api", "/"));
        assert!(!matches("https://example.com/", ""));
    }
}
