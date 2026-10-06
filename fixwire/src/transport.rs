//! Sending: one background thread takes requests from a bounded queue,
//! gzips and posts them, retries with backoff, and pauses the kinds of data
//! Fixwire asks it to (`Fixwire-Rate-Limits`, `Retry-After`).

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

/// The sends of one request in all (after no answer, a `5xx` or a `429`'s
/// pause), and the longest a request waits for its next try: one due later
/// is dropped.
const MAX_ATTEMPTS: u32 = 4;
const MAX_WAIT: Duration = Duration::from_secs(300);
/// The longest pause an answer may ask for (a day): a longer one would
/// overflow the clock.
const MAX_PAUSE: u64 = 86_400;

enum Message {
    Send(Request),
    Stop,
}

struct State {
    /// Requests queued or waiting to be retried.
    pending: usize,
    /// Paused until, per category (`None`: every category).
    until: HashMap<Option<Category>, Instant>,
    /// The thread has ended.
    stopped: bool,
}

struct Shared {
    state: Mutex<State>,
    /// Signalled when nothing is pending, and when the thread ends.
    idle: Condvar,
    /// Set by `close`: what is left is dropped, not sent.
    stopping: AtomicBool,
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

/// The first retry's wait, doubled for each one after (tests shorten it).
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
                stopped: false,
            }),
            idle: Condvar::new(),
            stopping: AtomicBool::new(false),
            debug: opts.debug,
        });
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(opts.timeout))
            .http_status_as_error(false)
            // A redirect is an answer, not followed: the key and the data go to the DSN's host only.
            .max_redirects(0)
            .user_agent(format!("{}/{}", crate::SDK_NAME, crate::SDK_VERSION))
            .build()
            .into();
        let max_delayed = opts.max_queue;
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
                        max_delayed,
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
            .unwrap_or_else(PoisonError::into_inner);
        drop(state);
        !result.timed_out()
    }

    /// Stops the thread; what is still queued is dropped. Waits up to
    /// `timeout` for a request being sent, then leaves the thread to end on
    /// its own.
    pub(crate) fn close(&self, timeout: Duration) {
        self.shared.stopping.store(true, Ordering::Relaxed);
        if let Some(sender) = lock(&self.sender).take() {
            let _ = sender.try_send(Message::Stop);
        }
        if let Some(worker) = lock(&self.worker).take()
            && worker.thread().id() != std::thread::current().id()
        {
            let state = lock(&self.shared.state);
            let (state, _) = self
                .shared
                .idle
                .wait_timeout_while(state, timeout, |s| !s.stopped)
                .unwrap_or_else(PoisonError::into_inner);
            if state.stopped {
                drop(state);
                let _ = worker.join();
            }
        }
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.close(Duration::ZERO);
    }
}

struct Worker {
    dsn: Dsn,
    agent: ureq::Agent,
    shared: Arc<Shared>,
    /// Requests waiting to be retried, the soonest first.
    delayed: BinaryHeap<Reverse<(Instant, u64, DelayedRequest)>>,
    /// Bounds `delayed`: the queue's size.
    max_delayed: usize,
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
        lock(&self.shared.state).stopped = true;
        self.shared.idle.notify_all();
    }

    fn later(&mut self, request: Request, wait: Duration) {
        if wait > MAX_WAIT {
            self.shared.log(|| {
                format!(
                    "dropping a {} request: its next try is {}s away",
                    request.category.name(),
                    wait.as_secs()
                )
            });
            self.shared.finish();
            return;
        }
        if self.delayed.len() >= self.max_delayed {
            self.shared.log(|| {
                format!(
                    "too many requests waiting, dropping a {} request",
                    request.category.name()
                )
            });
            self.shared.finish();
            return;
        }
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
        if self.shared.stopping.load(Ordering::Relaxed) {
            self.shared.finish();
            return;
        }
        let wait = self.paused_for(request.category, Instant::now());
        if !wait.is_zero() {
            self.later(request, wait);
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
        // About 1 s, then twice as long each time; longer when the answer asks.
        let backoff = BACKOFF_UNIT * 2u32.pow(request.attempts - 1);
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
            .and_then(|v| retry_after(&v, SystemTime::now()))
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
        // A 429 without Fixwire's limits pauses everything for at least a minute; a 5xx that
        // says how long, for that long.
        if status == 429 && limits.is_none() {
            let secs = retry_after.map_or(60, |d| d.as_secs().max(60));
            self.limit(&format!("{secs}:"), now);
        } else if status >= 500
            && let Some(d) = retry_after
        {
            self.limit(&format!("{}:", d.as_secs()), now);
        }
        Ok((status, retry_after))
    }

    /// Reads `Fixwire-Rate-Limits`: `<seconds>:<category;…>, …`, no
    /// categories meaning all of them.
    fn limit(&self, header: &str, now: Instant) {
        let mut state = lock(&self.shared.state);
        for part in header.split(',') {
            let (secs, categories) = part.trim().split_once(':').unwrap_or((part.trim(), ""));
            let Some(secs) = seconds(secs.trim()) else {
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

/// Seconds as answers write them, cut to a day (more digits than a `u64`
/// holds too); `None` when they aren't digits.
fn seconds(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(s.parse::<u64>().map_or(MAX_PAUSE, |n| n.min(MAX_PAUSE)))
}

/// The seconds a `Retry-After` asks to wait from `now`, as seconds or an
/// HTTP date, cut to a day; `None` when it is neither.
fn retry_after(value: &str, now: SystemTime) -> Option<u64> {
    let value = value.trim();
    if let Some(s) = seconds(value) {
        return Some(s);
    }
    let at = http_date(value)?;
    let now = now.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(at.saturating_sub(now).min(MAX_PAUSE))
}

/// An HTTP date as Unix seconds: `Sun, 06 Nov 1994 08:49:37 GMT`, or the
/// obsolete forms recipients still read, `Sunday, 06-Nov-94 08:49:37 GMT` and
/// `Sun Nov  6 08:49:37 1994`.
fn http_date(s: &str) -> Option<u64> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let tokens: Vec<&str> = s.split([' ', ',', '-']).filter(|t| !t.is_empty()).collect();
    let (day, month, year, time) = match tokens[..] {
        [_, day, month, year, time, "GMT"] => (day, month, year, time),
        [_, month, day, time, year] => (day, month, year, time),
        _ => return None,
    };
    let number = |s: &str, max: u64| s.parse::<u64>().ok().filter(|n| *n <= max);
    let month = MONTHS.iter().position(|m| *m == month)? as u64 + 1;
    let day = number(day, 31).filter(|d| *d > 0)?;
    let year = match number(year, 9999)? {
        y if y < 70 && year.len() == 2 => y + 2000,
        y if year.len() == 2 => y + 1900,
        y if y < 1970 => return None,
        y => y,
    };
    let mut hms = time.split(':');
    let (h, m, sec) = (hms.next()?, hms.next()?, hms.next()?);
    if hms.next().is_some() {
        return None;
    }
    let (h, m, sec) = (number(h, 23)?, number(m, 59)?, number(sec, 60)?);
    // Howard Hinnant's days_from_civil, from 1970 on.
    let (y, mo) = if month <= 2 {
        (year - 1, month + 9)
    } else {
        (year, month - 3)
    };
    let (era, yoe) = (y / 400, y % 400);
    let doy = (153 * mo + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = (era * 146_097 + doe).checked_sub(719_468)?;
    Some(days * 86_400 + h * 3_600 + m * 60 + sec)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn worker(max_delayed: usize) -> Worker {
        worker_to("http://k@127.0.0.1:9", max_delayed)
    }

    fn worker_to(dsn: &str, max_delayed: usize) -> Worker {
        Worker {
            dsn: dsn.parse().unwrap(),
            // As `Transport::new` sets it up: a 429 or a 5xx is an answer.
            agent: ureq::Agent::config_builder()
                .http_status_as_error(false)
                .build()
                .into(),
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    pending: 0,
                    until: HashMap::new(),
                    stopped: false,
                }),
                idle: Condvar::new(),
                stopping: AtomicBool::new(false),
                debug: false,
            }),
            delayed: BinaryHeap::new(),
            max_delayed,
            seq: 0,
        }
    }

    #[test]
    fn requests_waiting_out_a_pause_are_bounded() {
        let mut w = worker(10);
        w.limit("60:error", Instant::now());
        for _ in 0..1000 {
            lock(&w.shared.state).pending += 1;
            w.deliver(Request::json("/v1/logs", Category::Error, Vec::new()));
        }
        assert_eq!(w.delayed.len(), 10);
        assert_eq!(lock(&w.shared.state).pending, 10, "the rest are dropped");
    }

    #[test]
    fn retry_after_is_seconds_or_an_http_date_within_a_day() {
        // Sun, 06 Nov 1994 08:49:00 GMT
        let now = UNIX_EPOCH + Duration::from_secs(784_111_740);
        assert_eq!(
            http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
        for (value, secs) in [
            ("120", Some(120)),
            (" 7 ", Some(7)),
            ("86400", Some(86_400)),
            ("86401", Some(86_400)),
            ("99999999999999999999999", Some(86_400)),
            ("0", Some(0)),
            ("-1", None),
            ("1.5", None),
            ("soon", None),
            ("", None),
            ("Sun, 06 Nov 1994 08:49:37 GMT", Some(37)),
            ("Sunday, 06-Nov-94 08:49:37 GMT", Some(37)),
            ("Sun Nov  6 08:49:37 1994", Some(37)),
            ("Mon, 07 Nov 1994 08:49:01 GMT", Some(86_400)),
            ("Tue, 08 Nov 1994 08:49:00 GMT", Some(86_400)),
            ("Sun, 06 Nov 1994 08:48:00 GMT", Some(0)),
            ("Sun, 06 Nov 1994 08:49:37 CET", None),
            ("Sun, 32 Nov 1994 08:49:37 GMT", None),
            ("Sun, 06 Nov 1994 24:49:37 GMT", None),
            ("Sun, 06 Foo 1994 08:49:37 GMT", None),
            ("Sun, 06 Nov 1969 08:49:37 GMT", None),
        ] {
            assert_eq!(retry_after(value, now), secs, "{value:?}");
        }
        assert_eq!(http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(
            http_date("Sat, 29 Feb 2000 00:00:00 GMT"),
            Some(951_782_400)
        );
    }

    #[test]
    fn retries_wait_a_unit_then_twice_as_long_up_to_three_times() {
        let mut w = worker(10);
        lock(&w.shared.state).pending += 1;
        let mut request = Request::json("/v1/logs", Category::Error, Vec::new());
        for (attempt, unit) in [(1, 1), (2, 2), (3, 4)] {
            let before = Instant::now();
            w.retry(request, "503".into(), None);
            let Some(Reverse((due, _, DelayedRequest(r)))) = w.delayed.pop() else {
                panic!("retry {attempt} is waiting");
            };
            let wait = due - before;
            assert!(
                wait >= BACKOFF_UNIT * unit && wait < BACKOFF_UNIT * unit * 2,
                "{wait:?}"
            );
            request = r;
        }
        w.retry(request, "503".into(), None);
        assert!(w.delayed.is_empty(), "no fourth retry");
        assert_eq!(lock(&w.shared.state).pending, 0, "dropped");
    }

    /// A server answering each request with the next of `statuses`; the
    /// number of requests it got.
    fn answering(statuses: &'static [u16]) -> (String, Arc<Mutex<usize>>) {
        use std::io::{BufRead, BufReader, Read};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dsn = format!("http://k@{}", listener.local_addr().unwrap());
        let got = Arc::new(Mutex::new(0));
        let count = Arc::clone(&got);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut writer = stream.try_clone().unwrap();
                let mut reader = BufReader::new(stream);
                loop {
                    // The request line, the headers up to an empty line, the body.
                    let mut length = 0;
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    loop {
                        let mut h = String::new();
                        if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim_end().is_empty() {
                            break;
                        }
                        if let Some((k, v)) = h.split_once(':')
                            && k.eq_ignore_ascii_case("content-length")
                        {
                            length = v.trim().parse().unwrap_or(0);
                        }
                    }
                    let _ = reader.by_ref().take(length).read_to_end(&mut Vec::new());
                    let n = {
                        let mut got = lock(&count);
                        *got += 1;
                        *got
                    };
                    let status = statuses.get(n - 1).copied().unwrap_or(200);
                    let answer = format!("HTTP/1.1 {status} X\r\nContent-Length: 0\r\n\r\n");
                    if writer.write_all(answer.as_bytes()).is_err() {
                        break;
                    }
                }
            }
        });
        (dsn, got)
    }

    #[test]
    fn a_request_is_sent_at_most_four_times_429s_included() {
        let (dsn, got) = answering(&[429, 503, 429, 503]);
        let mut w = worker_to(&dsn, 10);
        lock(&w.shared.state).pending += 1;
        w.deliver(Request::json("/v1/logs", Category::Error, Vec::new()));
        assert!(!lock(&w.shared.state).until.is_empty(), "the 429 pauses");
        while let Some(Reverse((_, _, DelayedRequest(r)))) = w.delayed.pop() {
            // While a 429's pause lasts the request waits again, unsent; then the pause ends.
            w.deliver(r);
            lock(&w.shared.state).until.clear();
        }
        assert_eq!(*lock(&got), 4, "the fourth send is the last");
        assert_eq!(lock(&w.shared.state).pending, 0, "dropped");
    }

    #[test]
    fn a_next_try_past_five_minutes_drops_the_request() {
        let mut w = worker(10);
        lock(&w.shared.state).pending += 2;
        let r = || Request::json("/v1/logs", Category::Error, Vec::new());
        w.retry(r(), "503".into(), Some(Duration::from_secs(300)));
        assert_eq!(w.delayed.len(), 1, "five minutes wait");
        w.retry(r(), "503".into(), Some(Duration::from_secs(301)));
        assert_eq!(w.delayed.len(), 1);
        assert_eq!(lock(&w.shared.state).pending, 1, "the other is dropped");
    }

    #[test]
    fn only_known_categories_pause() {
        let w = worker(10);
        let now = Instant::now();
        w.limit(
            "60:log;file;metric_bucket, 30:span, x:error, 86401:session",
            now,
        );
        let state = lock(&w.shared.state);
        assert_eq!(state.until.len(), 2);
        assert_eq!(
            state.until[&Some(Category::Span)],
            now + Duration::from_secs(30)
        );
        assert_eq!(
            state.until[&Some(Category::Session)],
            now + Duration::from_secs(MAX_PAUSE)
        );
    }

    #[test]
    fn pauses_past_the_clock_are_bounded() {
        let w = worker(10);
        let now = Instant::now();
        w.limit("18446744073709551615:error, 99999999999999999:", now);
        let state = lock(&w.shared.state);
        assert_eq!(state.until.len(), 2);
        assert!(
            state
                .until
                .values()
                .all(|u| *u == now + Duration::from_secs(MAX_PAUSE))
        );
    }
}
