//! Hubs: a client with a stack of scopes, current per thread, and carried by
//! futures across the threads that poll them.

use std::any::type_name;
use std::cell::{Cell, RefCell};
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError, RwLock, TryLockError};
use std::task::{Context, Poll};
use std::time::Duration;

use crate::client::Client;
use crate::scope::Scope;
use crate::stacktrace;
use crate::types::{Breadcrumb, Event, Exception, Frame, Level, Mechanism};

/// A client with a stack of scopes. The crate's functions use the current
/// hub: the thread's (`Hub::run`, a future's `bind_hub`), else the
/// process's. A request gets a fork of it, with a scope of its own.
#[derive(Clone)]
pub struct Hub {
    inner: Arc<HubInner>,
}

struct HubInner {
    client: RwLock<Option<Arc<Client>>>,
    scopes: Mutex<Vec<Scope>>,
}

static PROCESS: LazyLock<Hub> = LazyLock::new(|| Hub::new(None, Scope::default()));

thread_local! {
    static THREAD: RefCell<Option<Hub>> = const { RefCell::new(None) };
    /// Set while the thread runs code that changes a scope: what that code
    /// captures (a panic in it, a log) doesn't wait for the lock it holds.
    static CONFIGURING: Cell<usize> = const { Cell::new(0) };
}

/// A lock that a panic while it was held doesn't spoil: the SDK must keep
/// working in a program that panicked.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Hub {
    /// A hub for a client and a scope.
    pub fn new(client: Option<Arc<Client>>, scope: Scope) -> Hub {
        Hub {
            inner: Arc::new(HubInner {
                client: RwLock::new(client),
                scopes: Mutex::new(vec![scope]),
            }),
        }
    }

    /// The current hub: the thread's, else the process's.
    pub fn current() -> Hub {
        // Gone while the thread ends: a destructor logging then gets the process's.
        THREAD
            .try_with(|t| t.borrow().clone())
            .ok()
            .flatten()
            .unwrap_or_else(Hub::main)
    }

    /// The process's hub, which `init` sets up.
    pub fn main() -> Hub {
        PROCESS.clone()
    }

    /// Runs `f` with `hub` as the thread's current hub.
    pub fn run<R>(hub: Hub, f: impl FnOnce() -> R) -> R {
        struct Restore(Option<Hub>);
        impl Drop for Restore {
            fn drop(&mut self) {
                let previous = self.0.take();
                THREAD.with(|t| *t.borrow_mut() = previous);
            }
        }
        let _restore = Restore(THREAD.with(|t| t.borrow_mut().replace(hub)));
        f()
    }

    /// A hub with the same client and a copy of the current scope, for work
    /// that runs apart (a request, a thread, a task).
    pub fn fork(&self) -> Hub {
        let scope = lock(&self.inner.scopes).last().cloned().unwrap_or_default();
        Hub::new(self.client(), scope)
    }

    /// The hub's client (`None` before `init`).
    pub fn client(&self) -> Option<Arc<Client>> {
        self.inner
            .client
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Makes `client` the hub's.
    pub fn bind_client(&self, client: Option<Arc<Client>>) {
        *self
            .inner
            .client
            .write()
            .unwrap_or_else(PoisonError::into_inner) = client;
    }

    /// Changes the current scope.
    pub fn configure_scope<R>(&self, f: impl FnOnce(&mut Scope) -> R) -> R {
        struct Configuring;
        impl Drop for Configuring {
            fn drop(&mut self) {
                CONFIGURING.with(|c| c.set(c.get() - 1));
            }
        }
        let mut scopes = lock(&self.inner.scopes);
        CONFIGURING.with(|c| c.set(c.get() + 1));
        let _configuring = Configuring;
        f(scopes.last_mut().expect("a hub always has a scope"))
    }

    /// The current scope for the SDK's own use: `None` when the thread is
    /// changing a scope and this one is locked (by that thread, perhaps), so
    /// the SDK never waits for a lock its own thread holds.
    fn try_configure_scope<R>(&self, f: impl FnOnce(&mut Scope) -> R) -> Option<R> {
        let mut scopes = if CONFIGURING.with(Cell::get) > 0 {
            match self.inner.scopes.try_lock() {
                Ok(scopes) => scopes,
                Err(TryLockError::Poisoned(p)) => p.into_inner(),
                Err(TryLockError::WouldBlock) => return None,
            }
        } else {
            lock(&self.inner.scopes)
        };
        scopes.last_mut().map(f)
    }

    /// Runs `f` with a copy of the current scope, set up by `configure`:
    /// what is set on it is gone afterwards.
    pub fn with_scope<R>(&self, configure: impl FnOnce(&mut Scope), f: impl FnOnce() -> R) -> R {
        struct Pop<'a>(&'a Hub);
        impl Drop for Pop<'_> {
            fn drop(&mut self) {
                let mut scopes = lock(&self.0.inner.scopes);
                if scopes.len() > 1 {
                    scopes.pop();
                }
            }
        }
        // `configure` runs without the lock: a panic in it is reported with the scope.
        let mut scope = lock(&self.inner.scopes).last().cloned().unwrap_or_default();
        configure(&mut scope);
        lock(&self.inner.scopes).push(scope);
        let _pop = Pop(self);
        f()
    }

    /// Sends an error, with the stack where it was captured and its chain of
    /// sources; its event id, or `None` when it was not sent.
    pub fn capture_error<E: Error + ?Sized>(&self, err: &E) -> Option<String> {
        // No stack is taken for a client that sends nothing.
        let client = self.client().filter(|c| c.is_enabled())?;
        let frames = stacktrace::capture(client.options());
        self.capture_event(Event {
            exceptions: exceptions_of(err, frames, Mechanism::default()),
            ..Event::default()
        })
    }

    /// Sends a message.
    pub fn capture_message(&self, message: impl Into<String>, level: Level) -> Option<String> {
        self.capture_event(Event {
            message: Some(message.into()),
            level: Some(level),
            ..Event::default()
        })
    }

    /// Sends an event as it is, with what the scope knows.
    pub fn capture_event(&self, mut event: Event) -> Option<String> {
        let client = self.client()?;
        if !client.is_enabled() {
            return None;
        }
        self.try_configure_scope(|scope| {
            scope.apply_to(&mut event);
            // The session counts the error whether or not it is sent.
            match event.exceptions.first() {
                Some(x) => scope.mark_session(!x.mechanism.handled),
                None if matches!(event.level, Some(Level::Error | Level::Fatal)) => {
                    scope.mark_session(false)
                }
                None => {}
            }
        });
        client.capture(event)
    }

    /// Records something that happened, on the current scope.
    pub fn add_breadcrumb(&self, breadcrumb: Breadcrumb) {
        let client = self.client();
        let (max, before) = client
            .as_ref()
            .map(|c| {
                (
                    c.options().max_breadcrumbs,
                    c.options().before_breadcrumb.clone(),
                )
            })
            .unwrap_or((100, None));
        let breadcrumb = match before {
            Some(f) => match f(breadcrumb) {
                Some(b) => b,
                None => return,
            },
            None => breadcrumb,
        };
        self.try_configure_scope(|s| s.add_breadcrumb(breadcrumb, max));
    }

    /// Waits until what was captured is sent, or `timeout`; false when time
    /// ran out.
    pub fn flush(&self, timeout: Duration) -> bool {
        self.client().is_none_or(|c| c.flush(timeout))
    }
}

impl fmt::Debug for Hub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Hub")
            .field("client", &self.client().is_some())
            .finish_non_exhaustive()
    }
}

/// Bounds the errors of a chain read from `source()`.
const MAX_CHAIN: usize = 10;

/// An error and its sources as exceptions, the outermost first; the
/// outermost gets `frames`, the stack where it was captured.
pub(crate) fn exceptions_of<E: Error + ?Sized>(
    err: &E,
    frames: Vec<Frame>,
    mechanism: Mechanism,
) -> Vec<Exception> {
    let (ty, module) = match static_type::<E>() {
        Some(t) => t,
        None => (debug_type(err), None),
    };
    let mut out = vec![Exception {
        ty,
        module,
        message: err.to_string(),
        mechanism,
        frames,
    }];
    let mut next = err.source();
    while let Some(e) = next {
        if out.len() >= MAX_CHAIN {
            break;
        }
        out.push(Exception {
            ty: debug_type(e),
            module: None,
            message: e.to_string(),
            mechanism: Mechanism {
                ty: "chained".into(),
                handled: out[0].mechanism.handled,
            },
            frames: Vec::new(),
        });
        next = e.source();
    }
    out
}

/// The error's type and its module, from the type it was captured as:
/// `ParseIntError` in `core::num::error`. `None` for a trait object.
fn static_type<E: ?Sized>() -> Option<(String, Option<String>)> {
    let name = type_name::<E>();
    if name.starts_with("dyn ") || name.starts_with('&') {
        return None;
    }
    let path = name.split('<').next().unwrap_or(name);
    Some(match path.rsplit_once("::") {
        Some((module, ty)) => (ty.to_owned(), Some(module.to_owned())),
        None => (path.to_owned(), None),
    })
}

/// A trait object's type, from its `Debug` form: `ParseIntError { kind:
/// InvalidDigit }` is a `ParseIntError`. Errors whose `Debug` is their
/// message are `Error`, and so is `std::io::Error`, whose `Debug` names its
/// representation (`Os { code: 2, kind: NotFound, … }`).
pub(crate) fn debug_type<E: fmt::Debug + ?Sized>(err: &E) -> String {
    let debug = format!("{err:?}");
    let ident: String = debug
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    let rest = &debug[ident.len()..];
    let io_error = match ident.as_str() {
        "Os" | "Custom" => rest.starts_with(" {") && rest.contains("kind: "),
        "Kind" => rest.starts_with('('),
        _ => false,
    };
    let looks_like_a_type = ident.starts_with(|c: char| c.is_ascii_uppercase())
        && (rest.is_empty() || rest.starts_with(['(', ' ', '{']));
    if looks_like_a_type && !io_error {
        ident
    } else {
        "Error".into()
    }
}

/// Carries a hub across the threads that poll a future: during each poll it
/// is the thread's current hub. A request's tasks get the request's hub.
pub trait FutureExt: Future + Sized {
    /// Polls the future with `hub` as the current hub.
    fn bind_hub(self, hub: Hub) -> HubBound<Self> {
        HubBound {
            hub,
            future: Box::pin(self),
        }
    }
}

impl<F: Future> FutureExt for F {}

/// A future with its hub; see `FutureExt::bind_hub`.
pub struct HubBound<F> {
    hub: Hub,
    future: Pin<Box<F>>,
}

impl<F: Future> Future for HubBound<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let hub = self.hub.clone();
        Hub::run(hub, || self.future.as_mut().poll(cx))
    }
}

impl<F> fmt::Debug for HubBound<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HubBound")
            .field("hub", &self.hub)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Declined;
    impl fmt::Display for Declined {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("card declined")
        }
    }
    impl Error for Declined {}

    #[derive(Debug)]
    struct Charge(Declined);
    impl fmt::Display for Charge {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("charging order 7")
        }
    }
    impl Error for Charge {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn names_errors_and_their_sources() {
        let chain = exceptions_of(&Charge(Declined), Vec::new(), Mechanism::default());
        let names: Vec<_> = chain
            .iter()
            .map(|x| (x.ty.as_str(), x.message.as_str(), x.mechanism.ty.as_str()))
            .collect();
        assert_eq!(
            names,
            [
                ("Charge", "charging order 7", "generic"),
                ("Declined", "card declined", "chained")
            ]
        );
        assert_eq!(chain[0].module.as_deref(), Some("fixwire::hub::tests"));

        let parse = "x".parse::<u32>().unwrap_err();
        let boxed: Box<dyn Error> = Box::new(parse);
        let chain = exceptions_of(&*boxed, Vec::new(), Mechanism::default());
        assert_eq!(chain[0].ty, "ParseIntError");
        assert_eq!(debug_type(&std::io::Error::other("boom")), "Error");
        assert_eq!(
            debug_type(&std::io::Error::from(std::io::ErrorKind::NotFound)),
            "Error"
        );
    }

    #[test]
    fn what_runs_while_the_scope_changes_never_waits_for_it() {
        let client = Client::new(crate::Options {
            dsn: Some("http://k@127.0.0.1:9".into()),
            ..crate::Options::default()
        })
        .unwrap();
        let hub = Hub::new(Some(Arc::new(client)), Scope::default());
        let (done, finished) = std::sync::mpsc::channel();
        let h = hub.clone();
        std::thread::spawn(move || {
            // What a log or a panic in there would do, through the tracing layer or the hook.
            let sent = h.configure_scope(|s| {
                s.set_tag("plan", "team");
                h.add_breadcrumb(Breadcrumb::new("user", "dropped: the scope is busy"));
                h.capture_message("sent without the scope", Level::Error)
            });
            let _ = done.send(sent.is_some());
        });
        assert_eq!(
            finished.recv_timeout(Duration::from_secs(5)),
            Ok(true),
            "no deadlock"
        );
        hub.add_breadcrumb(Breadcrumb::new("user", "kept"));
        assert_eq!(hub.configure_scope(|s| s.breadcrumbs.len()), 1);
    }
}
