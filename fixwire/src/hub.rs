//! Hubs: a client with a stack of scopes, current per thread, and carried by
//! futures across the threads that poll them.

use std::any::type_name;
use std::cell::{Cell, RefCell};
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
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
    /// Set while the thread captures: what the app's code logs or panics
    /// with then (a callback, a `Display`) isn't captured again.
    static CAPTURING: Cell<usize> = const { Cell::new(0) };
}

/// A lock that a panic while it was held doesn't spoil: the SDK must keep
/// working in a program that panicked.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether the thread is capturing: logging integrations and the panic
/// hook leave alone what happens meanwhile.
pub(crate) fn capturing() -> bool {
    CAPTURING.try_with(Cell::get).unwrap_or(0) > 0
}

/// Runs the SDK's work for an entry point, or the app's code it calls (a
/// callback, an error's `Display`), as capturing: a panic in it stays out of
/// the app's way (`None`), and isn't reported as a crash.
pub(crate) fn guarded<R>(f: impl FnOnce() -> R) -> Option<R> {
    struct Capturing;
    impl Drop for Capturing {
        fn drop(&mut self) {
            let _ = CAPTURING.try_with(|c| c.set(c.get().saturating_sub(1)));
        }
    }
    let _ = CAPTURING.try_with(|c| c.set(c.get() + 1));
    let _capturing = Capturing;
    std::panic::catch_unwind(AssertUnwindSafe(f)).ok()
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
    pub(crate) fn try_configure_scope<R>(&self, f: impl FnOnce(&mut Scope) -> R) -> Option<R> {
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
        // The stack is taken first: where it is captured, not the guard's frames.
        let frames = stacktrace::capture(client.options());
        let exceptions = guarded(|| exceptions_of(err, frames, Mechanism::default()))?;
        self.capture_event(Event {
            exceptions,
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
        guarded(|| {
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
        })
        .flatten()
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
            // A callback that panics is skipped: the breadcrumb is kept as it was.
            Some(f) => match guarded(|| f(breadcrumb.clone())) {
                Some(Some(b)) => b,
                Some(None) => return,
                None => {
                    if let Some(c) = &client {
                        c.log(|| {
                            "before_breadcrumb panicked: the breadcrumb is kept as it was".into()
                        });
                    }
                    breadcrumb
                }
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
pub(crate) const MAX_CHAIN: usize = 10;

/// An error and its sources as exceptions, the outermost first; the
/// outermost gets `frames`, the stack where it was captured. The chain ends
/// where it comes back to an error already in it. What the errors' own code
/// fails to give (a `Display` that panics) is `[Unreadable]`, or ends the
/// chain.
pub(crate) fn exceptions_of<E: Error + ?Sized>(
    err: &E,
    frames: Vec<Frame>,
    mechanism: Mechanism,
) -> Vec<Exception> {
    let message_of =
        |e: &dyn Error| guarded(|| e.to_string()).unwrap_or_else(|| "[Unreadable]".into());
    let type_of = |e: &dyn Error| guarded(|| debug_type(e)).unwrap_or_else(|| "Error".into());
    let (ty, module) = match static_type::<E>() {
        Some(t) => t,
        None => (
            guarded(|| debug_type(err)).unwrap_or_else(|| "Error".into()),
            None,
        ),
    };
    let mut out = vec![Exception {
        ty,
        module,
        message: guarded(|| err.to_string()).unwrap_or_else(|| "[Unreadable]".into()),
        mechanism,
        frames,
    }];
    // To tell when the chain comes back to an error: a source is known by its pointer (where it
    // is and its type), the outermost by where it is, its type's name and its message (a
    // struct's first field is where the struct is, and may be its source).
    let outer_at: *const () = (err as *const E).cast();
    let mut seen: Vec<*const (dyn Error + 'static)> = Vec::new();
    let mut next = guarded(|| err.source()).flatten();
    while let Some(e) = next {
        if out.len() >= MAX_CHAIN || seen.iter().any(|s| std::ptr::eq(*s, e)) {
            break;
        }
        let (ty, message) = (type_of(e), message_of(e));
        let at: *const () = (e as *const dyn Error).cast();
        if at == outer_at && ty == out[0].ty && message == out[0].message {
            break;
        }
        seen.push(e);
        out.push(Exception {
            ty,
            module: None,
            message,
            mechanism: Mechanism {
                ty: "chained".into(),
                handled: out[0].mechanism.handled,
            },
            frames: Vec::new(),
        });
        next = guarded(|| e.source()).flatten();
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

    /// An error whose source is the next one down, `n` deep.
    #[derive(Debug)]
    struct Nested(Option<Box<Nested>>, usize);
    impl fmt::Display for Nested {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "level {}", self.1)
        }
    }
    impl Error for Nested {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.0.as_deref().map(|e| e as &(dyn Error + 'static))
        }
    }

    /// Errors whose chains come back: to itself, and to the first of two.
    #[derive(Debug)]
    struct Again(u8);
    impl fmt::Display for Again {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "again {}", self.0)
        }
    }
    impl Error for Again {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(self)
        }
    }
    #[derive(Debug)]
    struct Ping(u8);
    #[derive(Debug)]
    struct Pong(u8);
    static PING: Ping = Ping(1);
    static PONG: Pong = Pong(2);
    impl fmt::Display for Ping {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "ping {}", self.0)
        }
    }
    impl fmt::Display for Pong {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "pong {}", self.0)
        }
    }
    impl Error for Ping {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(&PONG)
        }
    }
    impl Error for Pong {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(&PING)
        }
    }

    #[test]
    fn chains_stop_at_ten_and_where_they_come_back() {
        let eleven = (1..11).fold(Nested(None, 10), |e, n| Nested(Some(Box::new(e)), 10 - n));
        let chain = exceptions_of(&eleven, Vec::new(), Mechanism::default());
        assert_eq!(chain.len(), MAX_CHAIN);
        assert_eq!(
            (chain[0].message.as_str(), chain[9].message.as_str()),
            ("level 0", "level 9")
        );
        let names = |chain: &[Exception]| chain.iter().map(|x| x.ty.clone()).collect::<Vec<_>>();
        let chain = exceptions_of(&Again(0), Vec::new(), Mechanism::default());
        assert_eq!(names(&chain), ["Again"]);
        let chain = exceptions_of(&PING, Vec::new(), Mechanism::default());
        assert_eq!(names(&chain), ["Ping", "Pong"]);
        // Another Pong that says the same: one of the loop it leads to.
        let boxed: Box<dyn Error> = Box::new(Pong(2));
        let chain = exceptions_of(&*boxed, Vec::new(), Mechanism::default());
        assert_eq!(names(&chain), ["Pong", "Ping", "Pong"]);
    }

    #[test]
    fn errors_that_fail_to_print_are_unreadable() {
        #[derive(Debug)]
        struct Broken;
        impl fmt::Display for Broken {
            fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
                Err(fmt::Error) // makes to_string panic
            }
        }
        impl Error for Broken {}
        let chain = exceptions_of(&Charge(Declined), Vec::new(), Mechanism::default());
        assert_eq!(
            chain.len(),
            2,
            "a struct's first field is its source, not itself"
        );
        let chain = exceptions_of(&Broken, Vec::new(), Mechanism::default());
        assert_eq!(
            (chain[0].ty.as_str(), chain[0].message.as_str()),
            ("Broken", "[Unreadable]")
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
