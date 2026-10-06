//! Panics as crashes: a panic hook reports each one with the current hub's
//! scope, then runs the hook that was there before.

use std::cell::Cell;
use std::panic::PanicHookInfo;
use std::sync::Once;
use std::time::Duration;

use serde_json::{Map, Value};

use crate::hub::Hub;
use crate::stacktrace;
use crate::types::{Event, Exception, Level, Mechanism};

thread_local! {
    /// Set while a panic is reported, so a panic inside the report doesn't
    /// report itself.
    static REPORTING: Cell<bool> = const { Cell::new(false) };
}

/// Installs the hook once; the hook that was there keeps running after it.
pub(crate) fn install() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            report(info);
            previous(info);
        }));
    });
}

fn report(info: &PanicHookInfo<'_>) {
    // A panic of the app's code the SDK runs while capturing (a callback, a `Display`) is caught
    // there, not a crash.
    if crate::hub::capturing() || REPORTING.try_with(|r| r.replace(true)).unwrap_or(true) {
        return;
    }
    let hub = Hub::current();
    if let Some(client) = hub
        .client()
        .filter(|c| c.is_enabled() && c.options().capture_panics)
    {
        let message = match info.payload().downcast_ref::<&str>() {
            Some(s) => (*s).to_owned(),
            None => match info.payload().downcast_ref::<String>() {
                Some(s) => s.clone(),
                None => "Box<dyn Any>".to_owned(),
            },
        };
        let thread = std::thread::current();
        let mut context = Map::new();
        context.insert(
            "name".into(),
            Value::from(thread.name().unwrap_or("unnamed")),
        );
        let mut event = Event {
            level: Some(Level::Fatal),
            exceptions: vec![Exception {
                ty: "panic".into(),
                message,
                module: None,
                mechanism: Mechanism {
                    ty: "panic".into(),
                    handled: false,
                },
                frames: stacktrace::capture_panic(client.options()),
            }],
            ..Event::default()
        };
        event.contexts.insert("thread".into(), context);
        hub.try_configure_scope(|scope| {
            scope.apply_to(&mut event);
            scope.mark_session(true);
        });
        // A panic inside a panic hook ends the process, whatever would catch it: the event (and
        // the app's before_send) goes on a thread of its own, where one is caught.
        let sent = std::thread::scope(|s| {
            std::thread::Builder::new()
                .name("fixwire-panic".into())
                .spawn_scoped(s, || crate::hub::guarded(|| client.capture(event)))
                .is_ok_and(|t| t.join().is_ok_and(|done| done.is_some()))
        });
        if !sent {
            client.log(|| "a panic was not reported: its thread failed".into());
        }
        // The process ends now (panic = "abort"), or soon (the main thread): what was captured goes
        // first. Elsewhere the thread unwinds and the program goes on sending.
        if cfg!(panic = "abort") || thread.name() == Some("main") {
            client.flush(Duration::from_secs(2));
        }
    }
    let _ = REPORTING.try_with(|r| r.set(false));
}
