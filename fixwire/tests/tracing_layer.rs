//! The tracing layer: events become breadcrumbs, error events Fixwire
//! events.

mod common;

use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use common::{Ingest, attrs};
use fixwire::{Client, Hub, Options, Scope};
use serde_json::json;
use tracing_subscriber::layer::SubscriberExt;

#[test]
fn events_become_breadcrumbs_and_errors_events() {
    let ingest = Ingest::start();
    let client = Arc::new(
        Client::new(Options {
            dsn: Some(ingest.dsn()),
            project_root: Some(env!("CARGO_MANIFEST_DIR").into()),
            ..Options::default()
        })
        .unwrap(),
    );
    let hub = Hub::new(Some(Arc::clone(&client)), Scope::default());
    let subscriber = tracing_subscriber::registry().with(fixwire::tracing::layer());
    tracing::subscriber::with_default(subscriber, || {
        Hub::run(hub.clone(), || {
            tracing::info!(order = 7, "order received");
            tracing::debug!("not kept");
            let err = std::io::Error::other("disk full");
            tracing::error!(
                error = &err as &dyn Error,
                path = "/var/reports",
                "saving the report failed"
            );
            tracing::error!("the mail server said no to ada@example.com");
            // What tower-http's panic layer logs: the panic hook reported the panic already.
            tracing::error!(target: "tower_http::catch_panic", "Service panicked: attempt to divide by zero");
        });
    });
    assert!(client.flush(Duration::from_secs(5)));

    let records = ingest.records();
    assert_eq!(records.len(), 2);
    let failed = &records[0];
    let a = attrs(failed);
    assert_eq!(failed["eventName"], "exception");
    assert_eq!(failed["body"]["stringValue"], "saving the report failed");
    assert_eq!(
        (
            a["exception.type"].as_str(),
            a["exception.message"].as_str()
        ),
        (Some("Error"), Some("disk full"))
    );
    assert_eq!(a["fixwire.exceptions"][0]["mechanism"]["type"], "tracing");
    let frames = a["fixwire.exceptions"][0]["frames"].as_array().unwrap();
    let newest = frames
        .iter()
        .rev()
        .find(|f| f["in_app"] == json!(true))
        .unwrap();
    assert_eq!(
        newest["file"], "tests/tracing_layer.rs",
        "where it was logged, not tracing's frames"
    );
    assert_eq!(a["path"], "/var/reports");
    assert_eq!(a["logger"], "tracing_layer");
    let crumbs = a["fixwire.breadcrumbs"].as_array().unwrap();
    assert_eq!(crumbs.len(), 1, "info and up");
    assert_eq!(
        (
            crumbs[0]["message"].as_str(),
            crumbs[0]["data"]["order"].as_i64()
        ),
        (Some("order received"), Some(7))
    );

    let message = &records[1];
    assert_eq!(message["eventName"], "fixwire.message");
    assert_eq!(
        message["body"]["stringValue"],
        "the mail server said no to [REDACTED:email]"
    );
}
