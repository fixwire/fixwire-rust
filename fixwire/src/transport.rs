//! Sending: one background thread takes requests from a bounded queue,
//! gzips and posts them, retries with backoff, and pauses the kinds of data
//! Fixwire asks it to (`Fixwire-Rate-Limits`, `Retry-After`).

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::io::Write;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use flate2::Compression;
use flate2::write::GzEncoder;

use crate::dsn::Dsn;
use crate::hub::lock;
use crate::options::Options;

/// The kinds of data, as the protocol's rate limits name them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Category {
    Error,
    Span,
    Session,
    CheckIn,
    Feedback,
}

impl Category {
    fn name(self) -> &'static str {
        match self {
            Category::Error => "error",
            Category::Span => "span",
            Category::Session => "session",
            Category::CheckIn => "check_in",
            Category::Feedback => "feedback",
        }
    }
}

/// One request to Fixwire, kept until it is sent or dropped.
pub(crate) struct Request {
    pub(crate) path: String,
    pub(crate) category: Category,
    pub(crate) body: Vec<u8>,
    attempts: u32,
}

impl Request {
    pub(crate) fn json(path: impl Into<String>, category: Category, body: Vec<u8>) -> Request {
        Request {
            path: path.into(),
            category,
            body,
            attempts: 0,
        }
    }
}

/// The sends of one request, and the longest a paused request waits before
/// it is dropped.
const MAX_ATTEMPTS: u32 = 4;
const MAX_WAIT: Duration = Duration::from_secs(300);

enum Message {
    Send(Request),
    Stop,
}

struct State {
    /// Requests queued or waiting to be retried.
    pending: usize,
    /// Paused until, per category (`None`: every category).
    until: HashMap<Option<Category>, Instant>,
}

struct Shared {
    state: Mutex<State>,
    idle: Condvar,
    debug: bool,
}

impl Shared {
    fn log(&self, message: impl FnOnce() -> String) {
        if self.debug {
            eprintln!("fixwire: {}", message());
        }
    }

    fn finish(&self) {
        let mut state = lock(&self.state);
        state.pending = state.pending.saturating_sub(1);
        if state.pending == 0 {
            self.idle.notify_all();
        }
    }
}

/// Sends requests from a bounded queue on a thread of its own.
pub(crate) struct Transport {
    sender: Mutex<Option<SyncSender<Message>>>,
    shared: Arc<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

/// The first retry's wait, halved (tests shorten it).
#[cfg(not(test))]
const BACKOFF_UNIT: Duration = Duration::from_secs(1);
#[cfg(test)]
const BACKOFF_UNIT: Duration = Duration::from_millis(5);

impl Transport {
    pub(crate) fn new(dsn: Dsn, opts: &Options) -> Transport {
        let (sender, receiver) = sync_channel(opts.max_queue);
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                pending: 0,
                until: HashMap::new(),
            }),
            idle: Condvar::new(),
            debug: opts.debug,
        });
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(opts.timeout))
            .http_status_as_error(false)
            .user_agent(format!("{}/{}", crate::SDK_NAME, crate::SDK_VERSION))
            .build()
            .into();
        let worker = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("fixwire-transport".into())
                .spawn(move || {
                    Worker {
                        dsn,
                        agent,
                        shared,
                        delayed: BinaryHeap::new(),
                        seq: 0,
                    }
                    .run(receiver)
                })
                .ok()
        };
        Transport {
            sender: Mutex::new(Some(sender)),
            shared,
            worker: Mutex::new(worker),
        }
    }

    pub(crate) fn log(&self, message: impl FnOnce() -> String) {
        self.shared.log(message);
    }

    /// Queues a request; false when the queue is full or closed.
    pub(crate) fn send(&self, request: Request) -> bool {
        let sender = lock(&self.sender).clone();
        let Some(sender) = sender else {
            return false;
        };
        lock(&self.shared.state).pending += 1;
        let category = request.category;
        match sender.try_send(Message::Send(request)) {
            Ok(()) => true,
            Err(e) => {
                if matches!(e, TrySendError::Full(_)) {
                    self.log(|| format!("queue full, dropping a {} request", category.name()));
                }
                self.shared.finish();
                false
            }
        }
    }

    /// Waits until every queued request is sent or dropped, or `timeout`.
    pub(crate) fn flush(&self, timeout: Duration) -> bool {
        let state = lock(&self.shared.state);
        let (state, result) = self
            .shared
            .idle
            .wait_timeout_while(state, timeout, |s| s.pending > 0)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(state);
        !result.timed_out()
    }

    /// Stops the thread; what is still queued is dropped.
    pub(crate) fn close(&self) {
        if let Some(sender) = lock(&self.sender).take() {
            let _ = sender.try_send(Message::Stop);
        }
        if let Some(worker) = lock(&self.worker).take()
            && worker.thread().id() != std::thread::current().id()
        {
            let _ = worker.join();
        }
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.close();
    }
}

struct Worker {
    dsn: Dsn,
    agent: ureq::Agent,
    shared: Arc<Shared>,
    /// Requests waiting to be retried, the soonest first.
    delayed: BinaryHeap<Reverse<(Instant, u64, DelayedRequest)>>,
    seq: u64,
}

/// A request in the retry heap, ordered by when it is due (and the order it
/// came in).
struct DelayedRequest(Request);

impl PartialEq for DelayedRequest {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl Eq for DelayedRequest {}
impl PartialOrd for DelayedRequest {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for DelayedRequest {
    fn cmp(&self, _: &Self) -> std::cmp::Ordering {
        std::cmp::Ordering::Equal
    }
}

impl Worker {
    fn run(mut self, receiver: Receiver<Message>) {
        loop {
            // Whatever is due first: a retry, or the next request.
            let now = Instant::now();
            while let Some(Reverse((due, ..))) = self.delayed.peek() {
                if *due > now {
                    break;
                }
                let Some(Reverse((_, _, DelayedRequest(r)))) = self.delayed.pop() else {
                    break;
                };
                self.deliver(r);
            }
            let message = match self.delayed.peek() {
                Some(Reverse((due, ..))) => {
                    match receiver.recv_timeout(due.saturating_duration_since(Instant::now())) {
                        Ok(m) => m,
                        Err(RecvTimeoutError::Timeout) => continue,
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
                None => match receiver.recv() {
                    Ok(m) => m,
                    Err(_) => break,
                },
            };
            match message {
                Message::Send(r) => self.deliver(r),
                Message::Stop => break,
            }
        }
        // Dropped: count them done, so a flush doesn't wait for them.
        for _ in self.delayed.drain() {
            self.shared.finish();
        }
        while let Ok(Message::Send(_)) = receiver.try_recv() {
            self.shared.finish();
        }
    }

    fn later(&mut self, request: Request, wait: Duration) {
        self.seq += 1;
        self.delayed.push(Reverse((
            Instant::now() + wait,
            self.seq,
            DelayedRequest(request),
        )));
    }

    /// How long a category is still paused (zero when it isn't).
    fn paused_for(&self, category: Category, now: Instant) -> Duration {
        let state = lock(&self.shared.state);
        [Some(category), None]
            .iter()
            .filter_map(|c| state.until.get(c))
            .map(|until| until.saturating_duration_since(now))
            .max()
            .unwrap_or_default()
    }

    fn deliver(&mut self, request: Request) {
        let wait = self.paused_for(request.category, Instant::now());
        if !wait.is_zero() {
            if wait > MAX_WAIT {
                self.shared.log(|| {
                    format!(
                        "dropping a {} request: paused for {}s",
                        request.category.name(),
                        wait.as_secs()
                    )
                });
                self.shared.finish();
            } else {
                self.later(request, wait);
            }
            return;
        }
        match self.post(&request) {
            Ok((status, _)) if status < 300 => self.shared.finish(),
            Ok((status, retry_after)) if status == 429 || status >= 500 => {
                self.retry(request, status.to_string(), retry_after)
            }
            Err(e) => self.retry(request, e, None),
            Ok((status, _)) => {
                self.shared
                    .log(|| format!("{} request refused: {status}", request.category.name()));
                self.shared.finish();
            }
        }
    }

    fn retry(&mut self, mut request: Request, why: String, retry_after: Option<Duration>) {
        request.attempts += 1;
        if request.attempts >= MAX_ATTEMPTS {
            self.shared.log(|| {
                format!(
                    "dropping a {} request after {} attempts ({why})",
                    request.category.name(),
                    request.attempts
                )
            });
            self.shared.finish();
            return;
        }
        let backoff = BACKOFF_UNIT * 2u32.pow(request.attempts);
        self.later(request, backoff.max(retry_after.unwrap_or_default()));
    }

    /// Posts a request, gzipped, and reads the rate limits of the answer:
    /// its status and `Retry-After`.
    fn post(&self, request: &Request) -> Result<(u16, Option<Duration>), String> {
        let mut gz = GzEncoder::new(
            Vec::with_capacity(request.body.len() / 4 + 64),
            Compression::default(),
        );
        gz.write_all(&request.body).map_err(|e| e.to_string())?;
        let body = gz.finish().map_err(|e| e.to_string())?;
        let mut response = self
            .agent
            .post(&self.dsn.url(&request.path))
            .header("Authorization", &format!("Bearer {}", self.dsn.key()))
            .header("Content-Type", "application/json")
            .header("Content-Encoding", "gzip")
            .send(&body[..])
            .map_err(|e| e.to_string())?;
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let retry_after = header("Retry-After")
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|s| *s > 0)
            .map(Duration::from_secs);
        let limits = header("Fixwire-Rate-Limits");
        let status = response.status().as_u16();
        let _ = response
            .body_mut()
            .with_config()
            .limit(64 * 1024)
            .read_to_vec();
        let now = Instant::now();
        if let Some(limits) = &limits {
            self.limit(limits, now);
        }
        if status == 429 && limits.is_none() {
            let secs = retry_after.map_or(60, |d| d.as_secs().max(60));
            self.limit(&format!("{secs}:"), now);
        }
        Ok((status, retry_after))
    }

    /// Reads `Fixwire-Rate-Limits`: `<seconds>:<category;…>, …`, no
    /// categories meaning all of them.
    fn limit(&self, header: &str, now: Instant) {
        let mut state = lock(&self.shared.state);
        for part in header.split(',') {
            let (secs, categories) = part.trim().split_once(':').unwrap_or((part.trim(), ""));
            let Ok(secs) = secs.trim().parse::<u64>() else {
                continue;
            };
            if secs == 0 {
                continue;
            }
            let until = now + Duration::from_secs(secs);
            let names: Vec<Option<Category>> = if categories.trim().is_empty() {
                vec![None]
            } else {
                categories
                    .split(';')
                    .filter_map(|c| category_named(c.trim()).map(Some))
                    .collect()
            };
            for c in names {
                let entry = state.until.entry(c).or_insert(until);
                if until > *entry {
                    *entry = until;
                }
            }
        }
    }
}

fn category_named(name: &str) -> Option<Category> {
    Some(match name {
        "error" => Category::Error,
        "span" => Category::Span,
        "session" => Category::Session,
        "check_in" => Category::CheckIn,
        "feedback" => Category::Feedback,
        _ => return None,
    })
}
