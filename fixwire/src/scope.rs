//! What is known about the work under way, added to every event captured
//! with it.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::SystemTime;

use serde_json::{Map, Value};

use crate::sessions::RequestSession;
use crate::span::Span;
use crate::types::{Breadcrumb, Event, Level, Request, User};

/// The user, tags, contexts, breadcrumbs, request and span of the work under
/// way. A request gets a scope of its own (the tower layer gives it one);
/// `with_scope` makes a short-lived copy.
#[derive(Clone, Debug, Default)]
pub struct Scope {
    pub(crate) user: Option<User>,
    pub(crate) tags: BTreeMap<String, String>,
    pub(crate) contexts: BTreeMap<String, Map<String, Value>>,
    pub(crate) extra: Map<String, Value>,
    pub(crate) breadcrumbs: VecDeque<Breadcrumb>,
    pub(crate) level: Option<Level>,
    pub(crate) fingerprint: Vec<String>,
    pub(crate) transaction: Option<String>,
    pub(crate) request: Option<Request>,
    pub(crate) span: Option<Span>,
    pub(crate) session: Option<Arc<RequestSession>>,
}

impl Scope {
    /// Sets who the work is for; `None` forgets them.
    pub fn set_user(&mut self, user: Option<User>) {
        self.user = user;
    }

    /// The user, if set.
    pub fn user(&self) -> Option<&User> {
        self.user.as_ref()
    }

    /// Sets a searchable tag.
    pub fn set_tag(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.tags.insert(key.into(), value.into());
    }

    /// Removes a tag.
    pub fn remove_tag(&mut self, key: &str) {
        self.tags.remove(key);
    }

    /// Sets a named group of details, such as `order`; an empty map removes
    /// it.
    pub fn set_context(&mut self, name: impl Into<String>, values: Map<String, Value>) {
        let name = name.into();
        if values.is_empty() {
            self.contexts.remove(&name);
        } else {
            self.contexts.insert(name, values);
        }
    }

    /// Sets a detail sent as it is.
    pub fn set_extra(&mut self, key: impl Into<String>, value: impl Into<Value>) {
        self.extra.insert(key.into(), value.into());
    }

    /// Sets the level of the events captured with the scope.
    pub fn set_level(&mut self, level: Option<Level>) {
        self.level = level;
    }

    /// Groups the events captured with the scope your way.
    pub fn set_fingerprint<I: IntoIterator<Item = S>, S: Into<String>>(&mut self, fingerprint: I) {
        self.fingerprint = fingerprint.into_iter().map(Into::into).collect();
    }

    /// Names the route or task.
    pub fn set_transaction(&mut self, name: impl Into<String>) {
        self.transaction = Some(name.into());
    }

    /// Records the HTTP request the work serves.
    pub fn set_request(&mut self, request: Option<Request>) {
        self.request = request;
    }

    /// Makes `span` the scope's span: events captured with the scope link to
    /// its trace, and spans started with it are its children.
    pub fn set_span(&mut self, span: Option<Span>) {
        self.span = span;
    }

    /// The scope's span, if any.
    pub fn span(&self) -> Option<&Span> {
        self.span.as_ref()
    }

    /// Records something that happened; the oldest go past `max`.
    pub(crate) fn add_breadcrumb(&mut self, mut b: Breadcrumb, max: usize) {
        if max == 0 {
            return;
        }
        b.timestamp.get_or_insert_with(SystemTime::now);
        b.level.get_or_insert(Level::Info);
        self.breadcrumbs.push_back(b);
        while self.breadcrumbs.len() > max {
            self.breadcrumbs.pop_front();
        }
    }

    /// Forgets the breadcrumbs.
    pub fn clear_breadcrumbs(&mut self) {
        self.breadcrumbs.clear();
    }

    /// Fills in what the event doesn't say itself.
    pub(crate) fn apply_to(&self, e: &mut Event) {
        if e.user.as_ref().is_none_or(User::is_empty) {
            e.user.clone_from(&self.user);
        }
        for (k, v) in &self.tags {
            e.tags.entry(k.clone()).or_insert_with(|| v.clone());
        }
        for (k, v) in &self.contexts {
            e.contexts.entry(k.clone()).or_insert_with(|| v.clone());
        }
        for (k, v) in &self.extra {
            e.extra.entry(k.clone()).or_insert_with(|| v.clone());
        }
        if e.breadcrumbs.is_empty() {
            e.breadcrumbs = self.breadcrumbs.iter().cloned().collect();
        }
        if e.level.is_none() {
            e.level = self.level;
        }
        if e.fingerprint.is_empty() {
            e.fingerprint.clone_from(&self.fingerprint);
        }
        if e.transaction.is_none() {
            e.transaction = self.transaction.clone().or_else(|| {
                let r = self.request.as_ref()?;
                Some(format!("{} {}", r.method, r.route.as_ref()?))
            });
        }
        if e.request.is_none() {
            e.request.clone_from(&self.request);
        }
        if e.trace.is_none() {
            e.trace = self
                .span
                .as_ref()
                .map(|s| (s.trace_id().to_owned(), s.span_id().to_owned()));
        }
    }

    /// Records an error on the scope's request session: errored, or crashed
    /// when nothing handled it.
    pub(crate) fn mark_session(&self, crashed: bool) {
        if let Some(s) = &self.session {
            s.mark(crashed);
        }
    }
}
