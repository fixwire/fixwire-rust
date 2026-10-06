//! Panics as crashes. A file of its own: the panic hook is the process's.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::{Ingest, attrs};
use fixwire::{Hub, Options};
use serde_json::json;

fn parse_order(text: &str) -> u32 {
    if text.is_empty() {
        panic!("invalid order: {text:?}");
    }
    text.parse().unwrap() // a second way to panic
}

#[test]
fn panics_are_crashes_and_the_previous_hook_still_runs() {
    let previous_ran = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&previous_ran);
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        flag.store(true, Ordering::SeqCst);
        default(info);
    }));

    let ingest = Ingest::start();
    let _guard = fixwire::init(Options {
        dsn: Some(ingest.dsn()),
        release: Some("shop@1.0.0".into()),
        project_root: Some(env!("CARGO_MANIFEST_DIR").into()),
        ..Options::default()
    });
    let hub = Hub::main().fork();
    hub.configure_scope(|s| s.set_tag("job", "import"));

    for input in ["", "4x"] {
        let hub = hub.clone();
        let worker = std::thread::Builder::new()
            .name("importer".into())
            .spawn(move || Hub::run(hub, || parse_order(input)))
            .unwrap();
        assert!(worker.join().is_err());
    }
    assert!(
        previous_ran.load(Ordering::SeqCst),
        "the hook that was there ran too"
    );
    assert!(Hub::main().flush(Duration::from_secs(5)));

    let records = ingest.records();
    assert_eq!(records.len(), 2);
    for r in &records {
        assert_eq!(r["severityNumber"], 21, "fatal");
        let a = attrs(r);
        assert_eq!(a["fixwire.handled"], json!(false));
        assert_eq!(a["fixwire.tags"], json!({"job": "import"}));
        assert_eq!(a["fixwire.contexts"]["thread"]["name"], "importer");
        let x = &a["fixwire.exceptions"][0];
        assert_eq!(
            (x["type"].as_str(), x["mechanism"]["type"].as_str()),
            (Some("panic"), Some("panic"))
        );
        // The panic machinery is left out: the newest frame of the app is where it panicked.
        let frames = x["frames"].as_array().unwrap();
        let newest_app = frames
            .iter()
            .rev()
            .find(|f| f["in_app"] == json!(true))
            .unwrap();
        assert_eq!(newest_app["function"], "parse_order");
        assert_eq!(newest_app["file"], "tests/panics.rs");
        // Newer than it: only what `unwrap()` calls to panic, no hook or machinery.
        let at = frames.iter().rposition(|f| f == newest_app).unwrap();
        for f in &frames[at + 1..] {
            let module = f["module"]
                .as_str()
                .unwrap_or_default()
                .trim_start_matches('<');
            assert!(
                module.starts_with("core::result") || module.starts_with("core::option"),
                "{f}"
            );
        }
    }
    let messages: Vec<_> = records
        .iter()
        .map(|r| attrs(r)["exception.message"].clone())
        .collect();
    assert_eq!(messages[0], "invalid order: \"\"");
    assert_eq!(
        messages[1],
        "called `Result::unwrap()` on an `Err` value: ParseIntError { kind: InvalidDigit }"
    );
}
