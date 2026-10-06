//! What events carry: levels, exceptions with their frames, users,
//! breadcrumbs and requests.

use std::collections::BTreeMap;
use std::time::SystemTime;

use serde_json::{Map, Value};

/// An event's or a breadcrumb's severity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    /// Detail for debugging.
    Debug,
    /// Something worth knowing (the default for messages and breadcrumbs).
    #[default]
    Info,
    /// Something that may need attention.
    Warning,
    /// A failure (the default for errors).
    Error,
    /// A failure that ends the program, such as a panic.
    Fatal,
}

impl Level {
    /// The level as the protocol writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warning => "warning",
            Level::Error => "error",
            Level::Fatal => "fatal",
        }
    }

    /// OpenTelemetry's severity number.
    pub(crate) fn severity(self) -> u8 {
        match self {
            Level::Debug => 5,
            Level::Info => 9,
            Level::Warning => 13,
            Level::Error => 17,
            Level::Fatal => 21,
        }
    }
}

/// An error or a message as it is sent: `before_send` may change it.
#[derive(Clone, Debug, Default)]
pub struct Event {
    /// 32 hex characters; made when empty.
    pub event_id: String,
    /// When it happened; now when empty.
    pub timestamp: Option<SystemTime>,
    /// Error for errors and info for messages when not set.
    pub level: Option<Level>,
    /// A message's text (`capture_message`).
    pub message: Option<String>,
    /// An error's chain, the outermost first.
    pub exceptions: Vec<Exception>,
    /// Searchable tags.
    pub tags: BTreeMap<String, String>,
    /// Named groups of details, such as `order`.
    pub contexts: BTreeMap<String, Map<String, Value>>,
    /// Details sent as they are.
    pub extra: Map<String, Value>,
    /// Who it happened to.
    pub user: Option<User>,
    /// What happened before, oldest first.
    pub breadcrumbs: Vec<Breadcrumb>,
    /// Groups the event your way; `{{ default }}` stands for Fixwire's own
    /// grouping.
    pub fingerprint: Vec<String>,
    /// The route or task it happened in.
    pub transaction: Option<String>,
    /// The HTTP request it happened in.
    pub request: Option<Request>,
    /// The trace and span it happened in.
    pub trace: Option<(String, String)>,
    /// Occurrences the error budget held back since the last one sent.
    pub(crate) suppressed: u64,
}

/// One error of a chain.
#[derive(Clone, Debug, Default)]
pub struct Exception {
    /// Its type, such as `ParseIntError` or `panic`.
    pub ty: String,
    /// Its message.
    pub message: String,
    /// The module its type is in.
    pub module: Option<String>,
    /// How it was caught.
    pub mechanism: Mechanism,
    /// The stack, from the oldest call to the newest (the failing line
    /// last).
    pub frames: Vec<Frame>,
}

/// How an error was caught.
#[derive(Clone, Debug)]
pub struct Mechanism {
    /// `generic`, `panic`, `chained`, `tracing`, …
    pub ty: String,
    /// False for a crash: a panic, or an error nothing handled.
    pub handled: bool,
}

impl Default for Mechanism {
    fn default() -> Self {
        Mechanism {
            ty: "generic".into(),
            handled: true,
        }
    }
}

/// One stack frame.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Frame {
    /// The function, such as `charge` or `{{closure}}`.
    pub function: String,
    /// Its module path, such as `shop_api::cart`.
    pub module: Option<String>,
    /// Its file, relative to the project root when it is under it.
    pub file: Option<String>,
    /// Its file as the debug information names it.
    pub abs_path: Option<String>,
    /// Its line.
    pub line: Option<u32>,
    /// Its column.
    pub column: Option<u32>,
    /// Whether it is the app's code, not a dependency's or the standard
    /// library's.
    pub in_app: bool,
    /// The source line, when the file is there.
    pub context_line: Option<String>,
    /// The lines before it.
    pub pre_context: Vec<String>,
    /// The lines after it.
    pub post_context: Vec<String>,
}

/// Who the work is for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct User {
    /// Your id for them.
    pub id: Option<String>,
    /// Their email address.
    pub email: Option<String>,
    /// Their name.
    pub username: Option<String>,
    /// Their IP address: sent only with `send_default_pii`.
    pub ip_address: Option<String>,
}

impl User {
    /// A user known by your id for them.
    pub fn with_id(id: impl Into<String>) -> User {
        User {
            id: Some(id.into()),
            ..User::default()
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self == &User::default()
    }
}

/// Something that happened before an event.
#[derive(Clone, Debug, Default)]
pub struct Breadcrumb {
    /// When; now when empty.
    pub timestamp: Option<SystemTime>,
    /// `default`, `http`, `query`, `log`, …
    pub ty: Option<String>,
    /// What it is about, such as `cart` or `http`.
    pub category: Option<String>,
    /// What happened.
    pub message: Option<String>,
    /// Info when not set.
    pub level: Option<Level>,
    /// Details.
    pub data: Map<String, Value>,
}

impl Breadcrumb {
    /// A breadcrumb with a category and a message.
    pub fn new(category: impl Into<String>, message: impl Into<String>) -> Breadcrumb {
        Breadcrumb {
            category: Some(category.into()),
            message: Some(message.into()),
            ..Breadcrumb::default()
        }
    }
}

/// The HTTP request the work serves.
#[derive(Clone, Debug, Default)]
pub struct Request {
    /// GET, POST, …
    pub method: String,
    /// The URL without its query.
    pub url: String,
    /// The query string, without `?`.
    pub query: Option<String>,
    /// Headers: without `send_default_pii`, only those that can't identify
    /// the user.
    pub headers: BTreeMap<String, String>,
    /// The route it matched, such as `/orders/{id}`.
    pub route: Option<String>,
    /// The client's address, sent with `send_default_pii`.
    pub client_address: Option<String>,
}

/// Headers that may identify the user or carry a secret.
#[cfg_attr(not(feature = "tower"), allow(dead_code))]
pub(crate) fn sensitive_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization"
            | "proxy-authorization"
            | "cookie"
            | "set-cookie"
            | "x-api-key"
            | "x-forwarded-for"
            | "x-real-ip"
    )
}
