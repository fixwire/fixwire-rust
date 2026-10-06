//! The tower layer, on an axum app: each request has its own scope, a
//! server span named after its route and a session.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::Path;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use common::{Ingest, attrs};
use fixwire::{Client, FutureExt, Hub, Level, Options, Scope};
use serde_json::json;
use tower::ServiceExt;

async fn order(Path(id): Path<String>) -> (StatusCode, &'static str) {
    fixwire::set_tag("order", id.clone());
    match id.parse::<u32>() {
        Ok(7) => {
            fixwire::capture_message("order 7 looked odd", Level::Warning);
            (StatusCode::OK, "odd")
        }
        Ok(_) => (StatusCode::OK, "ok"),
        Err(e) => {
            fixwire::capture_error(&e);
            (StatusCode::BAD_REQUEST, "bad id")
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_get_a_scope_a_span_named_after_their_route_and_a_session() {
    let ingest = Ingest::start();
    let client = Arc::new(
        Client::new(Options {
            dsn: Some(ingest.dsn()),
            release: Some("shop@1.0.0".into()),
            traces_sample_rate: 1.0,
            session_interval: Duration::from_secs(3600),
            ..Options::default()
        })
        .unwrap(),
    );
    let hub = Hub::new(Some(Arc::clone(&client)), Scope::default());
    let app = Router::new()
        .route("/orders/{id}", get(order))
        .route(
            "/report",
            get(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
        )
        .layer(fixwire::tower::FixwireLayer::new());

    for (path, traceparent) in [
        (
            "/orders/1?expand=items",
            Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
        ),
        ("/orders/7", None),
        ("/orders/x", None),
        ("/report", None),
    ] {
        let mut req = Request::builder()
            .uri(path)
            .header("host", "shop.test")
            .header("user-agent", "tests")
            .header("cookie", "session=secret");
        if let Some(tp) = traceparent {
            req = req.header("traceparent", tp);
        }
        let res = app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .bind_hub(hub.clone())
            .await
            .unwrap();
        assert_ne!(res.status(), StatusCode::NOT_FOUND, "{path}");
    }
    assert!(client.flush(Duration::from_secs(5)));

    // A span per request, named after its route.
    let spans = ingest.spans();
    let names: Vec<_> = spans.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "GET /orders/{id}",
            "GET /orders/{id}",
            "GET /orders/{id}",
            "GET /report"
        ]
    );
    let first = &spans[0];
    assert_eq!(
        first["traceId"], "4bf92f3577b34da6a3ce929d0e0e4736",
        "continues the caller's trace"
    );
    assert_eq!(first["parentSpanId"], "00f067aa0ba902b7");
    let a = attrs(first);
    assert_eq!(a["http.route"], "/orders/{id}");
    assert_eq!(a["http.response.status_code"], 200);
    assert_eq!(a["server.address"], "shop.test");
    assert_eq!(spans[3]["status"]["code"], 2, "a 500 fails its span");

    // Events carry their request's scope, not the others'.
    let records = ingest.records();
    assert_eq!(records.len(), 2);
    let odd = records
        .iter()
        .find(|r| r["eventName"] == "fixwire.message")
        .unwrap();
    let a = attrs(odd);
    assert_eq!(a["fixwire.transaction"], "GET /orders/{id}");
    assert_eq!(a["fixwire.tags"], json!({"order": "7"}));
    assert_eq!(a["url.full"], "http://shop.test/orders/7");
    assert_eq!(a["user_agent.original"], "tests");
    assert!(
        a.get("http.request.header.cookie").is_none(),
        "no cookies without send_default_pii"
    );
    assert_eq!(odd["traceId"], spans[1]["traceId"]);
    let bad = records
        .iter()
        .find(|r| r["eventName"] == "exception")
        .unwrap();
    assert_eq!(attrs(bad)["exception.type"], "ParseIntError");
    assert!(
        hub.configure_scope(|s| s.span().is_none()),
        "the hub's own scope is untouched"
    );

    // Sessions: three requests went well (a warning and a 500 don't count as errors), one errored.
    let sessions = ingest.on("/v1/sessions");
    assert_eq!(sessions.len(), 1);
    let body = &sessions[0].body;
    let sum = |k: &str| {
        body["aggregates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a[k].as_u64().unwrap())
            .sum::<u64>()
    };
    assert_eq!((sum("exited"), sum("errored"), sum("crashed")), (3, 1, 0));
    assert_eq!(body["release"], "shop@1.0.0");
}
