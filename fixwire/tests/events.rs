//! What the SDK sends, checked at a fake ingest: errors with their chains,
//! frames and scope, messages, spans, check-ins, feedback, and how it backs
//! off when Fixwire asks it to.

mod common;

use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use common::{Ingest, attrs};
use fixwire::{Breadcrumb, Client, Feedback, Hub, Level, MonitorConfig, Options, Scope, User};
use serde_json::{Value, json};

/// A hub of its own, sending to `ingest`: tests run side by side.
fn hub(ingest: &Ingest, opts: Options) -> Hub {
    let client = Client::new(Options {
        dsn: Some(ingest.dsn()),
        release: Some("shop@1.0.0".into()),
        project_root: Some(env!("CARGO_MANIFEST_DIR").into()),
        ..opts
    })
    .unwrap();
    Hub::new(Some(Arc::new(client)), Scope::default())
}

fn flush(hub: &Hub) {
    assert!(hub.flush(Duration::from_secs(5)), "flushed in time");
}

#[derive(Debug)]
struct Declined {
    card: &'static str,
}
impl fmt::Display for Declined {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "card {} declined", self.card)
    }
}
impl Error for Declined {}

#[derive(Debug)]
struct ChargeFailed(Declined);
impl fmt::Display for ChargeFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("charging order 7 failed")
    }
}
impl Error for ChargeFailed {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}

fn charge_card() -> Result<(), ChargeFailed> {
    Err(ChargeFailed(Declined {
        card: "4111 1111 1111 1111",
    }))
}

#[test]
fn errors_go_with_their_chain_their_stack_and_the_scope() {
    let ingest = Ingest::start();
    let hub = hub(&ingest, Options::default());
    let id = Hub::run(hub.clone(), || {
        fixwire::set_user(Some(User {
            id: Some("user-1".into()),
            email: Some("ada@example.com".into()),
            ..User::default()
        }));
        fixwire::set_tag("plan", "team");
        fixwire::add_breadcrumb(Breadcrumb::new("cart", "checkout started"));
        let err = charge_card().unwrap_err();
        fixwire::capture_error(&err)
    })
    .expect("sent");
    flush(&hub);

    let records = ingest.records();
    assert_eq!(records.len(), 1);
    let r = &records[0];
    assert_eq!(
        (r["eventName"].as_str(), r["severityNumber"].as_i64()),
        (Some("exception"), Some(17))
    );
    let a = attrs(r);
    assert_eq!(a["fixwire.event_id"], json!(id));
    assert_eq!(
        (
            a["exception.type"].as_str(),
            a["exception.message"].as_str()
        ),
        (Some("ChargeFailed"), Some("charging order 7 failed"))
    );
    let chain = a["fixwire.exceptions"].as_array().unwrap();
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[1]["type"], "Declined");
    assert_eq!(
        chain[1]["message"], "card [REDACTED:credit_card] declined",
        "masked on the device"
    );
    assert_eq!(chain[1]["mechanism"]["type"], "chained");

    // The newest frame of the app is this test, relative to the project, with its source line.
    let frames = chain[0]["frames"].as_array().unwrap();
    let mine = frames
        .iter()
        .rev()
        .find(|f| f["in_app"] == json!(true))
        .expect("an in-app frame");
    assert_eq!(mine["function"], "{closure}");
    assert_eq!(
        mine["module"],
        "events::errors_go_with_their_chain_their_stack_and_the_scope"
    );
    assert_eq!(mine["file"], "tests/events.rs");
    assert!(
        mine["context_line"]
            .as_str()
            .unwrap()
            .contains("capture_error"),
        "{mine}"
    );
    assert!(
        frames.iter().all(|f| !f["module"]
            .as_str()
            .unwrap_or_default()
            .starts_with("fixwire::")),
        "no SDK frames"
    );
    assert!(
        frames.iter().any(|f| f["in_app"] == json!(false)),
        "the standard library's frames are not the app's"
    );

    assert_eq!(a["fixwire.tags"], json!({"plan": "team"}));
    assert_eq!(a["user.id"], "user-1");
    assert_eq!(a["user.email"], "[REDACTED:email]");
    assert_eq!(a["fixwire.breadcrumbs"][0]["message"], "checkout started");
    assert!(a.get("fixwire.handled").is_none(), "handled by default");

    let resource = attrs(&ingest.on("/v1/logs")[0].body["resourceLogs"][0]["resource"]);
    assert_eq!(resource["telemetry.sdk.name"], "fixwire.rust");
    assert_eq!(resource["service.name"], "shop");
    assert_eq!(resource["service.version"], "shop@1.0.0");
    let req = &ingest.on("/v1/logs")[0];
    assert_eq!(req.header("Authorization"), Some("Bearer fw_pk_test_rust"));
    assert_eq!(req.header("Content-Encoding"), Some("gzip"));
}

#[test]
fn messages_scopes_and_before_send() {
    let ingest = Ingest::start();
    let hub = hub(
        &ingest,
        Options {
            before_send: Some(Arc::new(|mut e: fixwire::Event| {
                if e.message.as_deref() == Some("drop me") {
                    return None;
                }
                e.tags.insert("seen".into(), "yes".into());
                Some(e)
            })),
            ..Options::default()
        },
    );
    Hub::run(hub.clone(), || {
        fixwire::with_scope(
            |s| s.set_transaction("nightly-report"),
            || fixwire::capture_message("report sent to ada@example.com", Level::Warning),
        );
        assert!(fixwire::capture_message("drop me", Level::Info).is_none());
    });
    flush(&hub);
    let records = ingest.records();
    assert_eq!(records.len(), 1);
    let r = &records[0];
    assert_eq!(r["eventName"], "fixwire.message");
    assert_eq!(r["severityNumber"], 13);
    assert_eq!(r["body"]["stringValue"], "report sent to [REDACTED:email]");
    let a = attrs(r);
    assert_eq!(a["fixwire.transaction"], "nightly-report");
    assert_eq!(a["fixwire.tags"], json!({"seen": "yes"}));
    let after = Hub::run(hub.clone(), || {
        fixwire::configure_scope(|s| s.span().is_none())
    });
    assert!(after, "with_scope's scope is gone");
}

#[test]
fn a_crash_loop_costs_a_burst_and_a_count() {
    let ingest = Ingest::start();
    let hub = hub(&ingest, Options::default());
    Hub::run(hub.clone(), || {
        for _ in 0..30 {
            fixwire::capture_error(&charge_card().unwrap_err());
        }
    });
    flush(&hub);
    assert_eq!(ingest.records().len(), 10);
}

#[test]
fn spans_go_with_their_segment() {
    let ingest = Ingest::start();
    let hub = hub(
        &ingest,
        Options {
            traces_sample_rate: 1.0,
            ..Options::default()
        },
    );
    Hub::run(hub.clone(), || {
        let request = fixwire::Span::continue_trace(
            Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
            None,
            None,
            "POST /orders",
            "http.server",
            hub.client(),
        );
        fixwire::configure_scope(|s| s.set_span(Some(request.clone())));
        let n = fixwire::trace("SELECT orders", "db.query", |span| {
            span.set_attribute("db.system.name", "postgresql");
            fixwire::trace("encode", "serialize", |_| 3)
        });
        assert_eq!(n, 3);
        assert!(
            ingest.on("/v1/traces").is_empty(),
            "children wait for their segment"
        );
        fixwire::capture_message("in the trace", Level::Info);
        request.finish();
    });
    flush(&hub);
    let spans = ingest.spans();
    assert_eq!(spans.len(), 3);
    let by_name = |n: &str| spans.iter().find(|s| s["name"] == n).unwrap().clone();
    let (root, query, encode) = (
        by_name("POST /orders"),
        by_name("SELECT orders"),
        by_name("encode"),
    );
    assert_eq!(root["traceId"], "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(root["parentSpanId"], "00f067aa0ba902b7");
    assert_eq!(
        root["flags"],
        0x100 | 0x200 | 1,
        "a segment with a remote parent, sampled"
    );
    assert_eq!(query["parentSpanId"], root["spanId"]);
    assert_eq!(encode["parentSpanId"], query["spanId"]);
    assert_eq!(
        (root["kind"].as_i64(), query["kind"].as_i64()),
        (Some(2), Some(3))
    );
    assert_eq!(attrs(&query)["fixwire.op"], "db.query");
    let message = &ingest.records()[0];
    assert_eq!(
        (message["traceId"].clone(), message["spanId"].clone()),
        (root["traceId"].clone(), root["spanId"].clone())
    );
}

#[test]
fn unsampled_traces_send_nothing_but_carry_on() {
    let ingest = Ingest::start();
    let hub = hub(
        &ingest,
        Options {
            traces_sample_rate: 0.0,
            ..Options::default()
        },
    );
    let traceparent = Hub::run(hub.clone(), || {
        fixwire::trace("job", "task", |s| s.traceparent())
    });
    flush(&hub);
    assert!(traceparent.ends_with("-00"));
    assert!(ingest.on("/v1/traces").is_empty());
}

#[test]
fn check_ins_and_feedback() {
    let ingest = Ingest::start();
    let hub = hub(&ingest, Options::default());
    let result: Result<(), std::io::Error> = Hub::run(hub.clone(), || {
        fixwire::with_monitor(
            "nightly report",
            Some(MonitorConfig::crontab("0 3 * * *")),
            || Err(std::io::Error::other("the mail server is down")),
        )
    });
    assert!(result.is_err());
    Hub::run(hub.clone(), || {
        fixwire::set_user(Some(User {
            email: Some("ada@example.com".into()),
            ..User::default()
        }));
        assert!(
            fixwire::capture_feedback(Feedback {
                message: Some("  ".into()),
                ..Feedback::default()
            })
            .is_none()
        );
        fixwire::capture_feedback(Feedback {
            message: Some("The refund was wrong, mail ada@example.com".into()),
            score: Some(-3.0),
            trace_id: Some("4bf92f3577b34da6a3ce929d0e0e4736".into()),
            ..Feedback::default()
        });
    });
    flush(&hub);
    let check_ins = ingest.on("/v1/check-ins/nightly%20report");
    assert_eq!(check_ins.len(), 2);
    let (start, end) = (&check_ins[0].body, &check_ins[1].body);
    assert_eq!(start["status"], "in_progress");
    assert_eq!(
        start["monitor_config"]["schedule"],
        json!({"type": "crontab", "value": "0 3 * * *"})
    );
    assert_eq!(end["status"], "error");
    assert_eq!(end["check_in_id"], start["check_in_id"]);
    assert!(end["duration"].as_f64().unwrap() > 0.0, "a measured run");
    assert!(end.get("monitor_config").is_none());

    let feedback = ingest.on("/v1/feedback");
    assert_eq!(feedback.len(), 1);
    let f = &feedback[0].body;
    assert_eq!(f["score"], -1.0);
    assert_eq!(f["message"], "The refund was wrong, mail [REDACTED:email]");
    assert_eq!(f["email"], "[REDACTED:email]");
    assert_eq!(f["trace_id"], "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(f["sdk"]["name"], "fixwire.rust");

    let ok = Hub::run(hub.clone(), || {
        fixwire::with_monitor("hourly", None, || Ok::<_, std::io::Error>(7))
    });
    assert_eq!(ok.unwrap(), 7);
    flush(&hub);
    assert_eq!(
        ingest.on("/v1/check-ins/hourly").last().unwrap().body["status"],
        "ok"
    );
}

#[test]
fn retries_what_failed_and_pauses_what_is_limited() {
    let ingest = Ingest::start();
    // The first answer fails; the retry goes through.
    ingest.answer([(503, vec![])]);
    let hub = hub(&ingest, Options::default());
    Hub::run(hub.clone(), || {
        fixwire::capture_message("first", Level::Info)
    });
    flush(&hub);
    assert_eq!(ingest.on("/v1/logs").len(), 2, "sent again after a 503");

    // Errors are paused for a minute: what follows waits, feedback still goes.
    ingest.answer([(200, vec![("Fixwire-Rate-Limits", "60:error".into())])]);
    Hub::run(hub.clone(), || {
        fixwire::capture_message("limited", Level::Info);
    });
    ingest.wait_for("/v1/logs", 3);
    Hub::run(hub.clone(), || {
        fixwire::capture_message("waits", Level::Info);
        fixwire::capture_feedback(Feedback {
            score: Some(1.0),
            ..Feedback::default()
        });
    });
    ingest.wait_for("/v1/feedback", 1);
    assert!(
        !hub.flush(Duration::from_millis(300)),
        "the paused event is still waiting"
    );
    assert_eq!(ingest.on("/v1/logs").len(), 3);
    assert_eq!(ingest.on("/v1/feedback").len(), 1);
}

#[test]
fn pauses_past_the_clock_are_cut_to_a_day() {
    let ingest = Ingest::start();
    // Seconds the clock can't add: the sending thread lives on, errors wait a day.
    ingest.answer([
        (
            200,
            vec![("Fixwire-Rate-Limits", "18446744073709551615:error".into())],
        ),
        (503, vec![("Retry-After", "18446744073709551615".into())]),
    ]);
    let hub = hub(&ingest, Options::default());
    let feedback = |score| {
        Hub::run(hub.clone(), || {
            fixwire::capture_feedback(Feedback {
                score: Some(score),
                ..Feedback::default()
            })
        })
    };
    Hub::run(hub.clone(), || {
        fixwire::capture_message("paused for a day", Level::Info)
    });
    ingest.wait_for("/v1/logs", 1);
    feedback(1.0); // retried in a day
    ingest.wait_for("/v1/feedback", 1);
    feedback(-1.0);
    assert_eq!(ingest.wait_for("/v1/feedback", 2).len(), 2);
}

#[test]
fn redirects_are_not_followed() {
    let elsewhere = Ingest::start();
    let ingest = Ingest::start();
    let location = format!("http://127.0.0.1:{}/v1/logs", elsewhere.port);
    ingest.answer([(307, vec![("Location", location)])]);
    let hub = hub(&ingest, Options::default());
    Hub::run(hub.clone(), || {
        fixwire::capture_message("stays home", Level::Info)
    });
    flush(&hub);
    assert_eq!(ingest.on("/v1/logs").len(), 1);
    assert!(
        elsewhere.received().is_empty(),
        "neither the key nor the data go elsewhere"
    );
}

#[test]
fn closing_takes_no_longer_than_asked() {
    // A server that takes requests and never answers.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _held: Vec<_> = listener.incoming().collect();
    });
    let client = Arc::new(
        Client::new(Options {
            dsn: Some(format!("http://k@127.0.0.1:{port}")),
            error_budget: fixwire::ErrorBudget {
                disabled: true,
                ..Default::default()
            },
            ..Options::default()
        })
        .unwrap(),
    );
    let hub = Hub::new(Some(Arc::clone(&client)), Scope::default());
    Hub::run(hub, || {
        for _ in 0..150 {
            fixwire::capture_message("never answered", Level::Info);
        }
    });
    let start = std::time::Instant::now();
    client.close(Duration::from_millis(200));
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn events_over_a_megabyte_go_without_their_extras_or_not_at_all() {
    let ingest = Ingest::start();
    let hub = hub(&ingest, Options::default());
    let huge = "x".repeat(1_100_000);
    let (smaller, dropped) = Hub::run(hub.clone(), || {
        fixwire::configure_scope(|s| s.set_extra("dump", huge.clone()));
        let smaller = fixwire::capture_message("the report is too large", Level::Error);
        fixwire::configure_scope(|s| s.set_extra("dump", Value::Null));
        (smaller, fixwire::capture_message(huge, Level::Error))
    });
    flush(&hub);
    assert!(smaller.is_some());
    assert_eq!(dropped, None);
    let records = ingest.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["body"]["stringValue"], "the report is too large");
    assert!(!attrs(&records[0]).contains_key("dump"));
}

#[test]
fn without_a_dsn_nothing_is_sent() {
    let client = Client::new(Options {
        dsn: Some(String::new()),
        ..Options::default()
    })
    .unwrap();
    if std::env::var("FIXWIRE_DSN").is_err() {
        assert!(!client.is_enabled());
    }
    assert!(
        Client::new(Options {
            dsn: Some("ingest.fixwire.io".into()),
            ..Options::default()
        })
        .is_err()
    );
    let value: Value = json!(null);
    assert!(value.is_null());
}
