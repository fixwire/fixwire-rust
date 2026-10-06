//! A tower layer for HTTP servers (axum, hyper, tonic): each request gets a
//! fork of the current hub with a scope of its own (the request on it), a
//! server span continuing the caller's trace, and a session; panics in the
//! handler are reported with all of that.
//!
//! ```ignore
//! let app = axum::Router::new()
//!     .route("/orders/{id}", axum::routing::get(order))
//!     .layer(fixwire::tower::FixwireLayer::new()); // after the routes: requests are named after them
//! ```
//!
//! With the `axum` feature, a request is named after the route it matched
//! (`GET /orders/{id}`): add the layer with `Router::layer`, which runs
//! after routing.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use pin_project_lite::pin_project;

use crate::hub::Hub;
use crate::sessions::RequestSessionGuard;
use crate::span::Span;
use crate::types::{Request, sensitive_header};

/// The layer; see the module's documentation.
#[derive(Clone, Copy, Debug, Default)]
pub struct FixwireLayer {
    _private: (),
}

impl FixwireLayer {
    /// The layer.
    pub fn new() -> FixwireLayer {
        FixwireLayer::default()
    }
}

impl<S> ::tower::Layer<S> for FixwireLayer {
    type Service = FixwireService<S>;

    fn layer(&self, inner: S) -> FixwireService<S> {
        FixwireService { inner }
    }
}

/// The service the layer wraps around a server's.
#[derive(Clone, Debug)]
pub struct FixwireService<S> {
    inner: S,
}

impl<S, B, R> ::tower::Service<http::Request<B>> for FixwireService<S>
where
    S: ::tower::Service<http::Request<B>, Response = http::Response<R>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<B>) -> ResponseFuture<S::Future> {
        let hub = Hub::current().fork();
        let client = hub.client();
        let send_pii = client
            .as_ref()
            .is_some_and(|c| c.options().send_default_pii);
        let header = |name: &str| req.headers().get(name).and_then(|v| v.to_str().ok());

        let method = req.method().as_str().to_owned();
        let scheme = req
            .uri()
            .scheme_str()
            .or_else(|| header("x-forwarded-proto"))
            .unwrap_or("http")
            .to_owned();
        let host = req
            .uri()
            .authority()
            .map(|a| a.as_str().to_owned())
            .or_else(|| header("host").map(str::to_owned));
        let path = req.uri().path().to_owned();
        let route = matched_route(&req);

        let name = match &route {
            Some(r) => format!("{method} {r}"),
            None => method.clone(),
        };
        let span = Span::continue_trace(
            header("traceparent"),
            header("tracestate"),
            header("baggage"),
            name,
            "http.server",
            client,
        );
        span.set_attribute("http.request.method", method.clone());
        span.set_attribute("url.path", path.clone());
        span.set_attribute("url.scheme", scheme.clone());
        if let Some(h) = &host {
            span.set_attribute("server.address", h.clone());
        }
        if let Some(ua) = header("user-agent") {
            span.set_attribute("user_agent.original", ua);
        }
        if let Some(r) = &route {
            span.set_attribute("http.route", r.clone());
        }

        let headers = req
            .headers()
            .iter()
            .filter(|(name, _)| send_pii || !sensitive_header(name.as_str()))
            .filter_map(|(name, v)| Some((canonical(name.as_str()), v.to_str().ok()?.to_owned())))
            .collect();
        let client_address = send_pii
            .then(|| {
                header("x-forwarded-for")
                    .and_then(|f| f.split(',').next())
                    .or_else(|| header("x-real-ip"))
                    .map(|ip| ip.trim().to_owned())
            })
            .flatten();
        let request = Request {
            url: format!("{scheme}://{}{path}", host.unwrap_or_default()),
            query: req.uri().query().map(str::to_owned),
            method,
            headers,
            route,
            client_address,
        };
        hub.configure_scope(|s| {
            s.set_request(Some(request));
            s.set_span(Some(span.clone()));
        });
        let session = hub.start_request_session();
        let inner = Hub::run(hub.clone(), || self.inner.call(req));
        ResponseFuture {
            inner,
            hub,
            state: Some(Ending {
                span,
                _session: session,
                ended: false,
            }),
        }
    }
}

/// The route the request matched, with the `axum` feature.
#[cfg(feature = "axum")]
fn matched_route<B>(req: &http::Request<B>) -> Option<String> {
    req.extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|m| m.as_str().to_owned())
}

#[cfg(not(feature = "axum"))]
fn matched_route<B>(_: &http::Request<B>) -> Option<String> {
    None
}

/// `content-type` as `Content-Type`.
fn canonical(name: &str) -> String {
    name.split('-')
        .map(|part| {
            let mut c = part.chars();
            c.next()
                .map(|f| f.to_ascii_uppercase().to_string() + c.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join("-")
}

pin_project! {
    /// The response of a request the layer serves.
    pub struct ResponseFuture<F> {
        #[pin]
        inner: F,
        hub: Hub,
        state: Option<Ending>,
    }
}

/// What ends with the request: its span and its session (when dropped).
struct Ending {
    span: Span,
    _session: RequestSessionGuard,
    ended: bool,
}

impl Ending {
    fn end(&mut self, status: Option<u16>) {
        self.ended = true;
        match status {
            Some(s) => {
                self.span.set_attribute("http.response.status_code", s);
                if s >= 500 {
                    self.span.set_error(s.to_string());
                }
            }
            None => self.span.set_error("the service failed"),
        }
        self.span.finish();
    }
}

impl Drop for Ending {
    fn drop(&mut self) {
        // The response never came: the handler panicked, or the client went away.
        if !self.ended {
            self.span.set_error(if std::thread::panicking() {
                "panicked"
            } else {
                "cancelled"
            });
            self.span.finish();
        }
    }
}

impl<F, R, E> Future for ResponseFuture<F>
where
    F: Future<Output = Result<http::Response<R>, E>>,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let this = self.project();
        let inner = this.inner;
        let result = Hub::run(this.hub.clone(), || inner.poll(cx));
        if let Poll::Ready(output) = &result
            && let Some(mut ending) = this.state.take()
        {
            ending.end(output.as_ref().ok().map(|r| r.status().as_u16()));
        }
        result
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn writes_header_names_canonically() {
        assert_eq!(super::canonical("x-request-id"), "X-Request-Id");
        assert_eq!(super::canonical("user-agent"), "User-Agent");
    }
}
