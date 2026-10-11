//! In-memory request log: printed to the terminal as requests happen and served
//! as a snapshot by `GET /requests`. Nothing is written to disk and prompts or
//! replies are never stored, only metadata.

use chrono::{DateTime, Local, SecondsFormat};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::IsTerminal,
    sync::{Arc, Mutex, MutexGuard},
    time::Instant,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Running,
    Ok,
    Failed,
    Cancelled,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Ok => "ok",
            Status::Failed => "failed",
            Status::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Status> {
        match s {
            "running" => Some(Status::Running),
            "ok" => Some(Status::Ok),
            "failed" => Some(Status::Failed),
            "cancelled" => Some(Status::Cancelled),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct Entry {
    pub id: u64,
    pub at: DateTime<Local>,
    started: Instant,
    pub client: String,
    pub agent: String,
    pub model: String,
    pub stream: bool,
    pub cwd: String,
    pub session: Option<String>,
    pub resumed: bool,
    pub prompt_chars: usize,
    pub status: Status,
    pub http_status: Option<u16>,
    pub duration_ms: Option<u64>,
    pub first_output_ms: Option<u64>,
    pub output_chars: usize,
    pub tokens: Option<(u64, u64)>,
    pub error: Option<String>,
}

impl Entry {
    fn new(id: u64, client: String) -> Self {
        Entry {
            id,
            at: Local::now(),
            started: Instant::now(),
            client,
            agent: String::new(),
            model: String::new(),
            stream: false,
            cwd: String::new(),
            session: None,
            resumed: false,
            prompt_chars: 0,
            status: Status::Running,
            http_status: None,
            duration_ms: None,
            first_output_ms: None,
            output_chars: 0,
            tokens: None,
            error: None,
        }
    }

    /// Record that `chars` characters of reply arrived.
    pub fn mark_output(&mut self, chars: usize) {
        if self.first_output_ms.is_none() {
            self.first_output_ms = Some(self.started.elapsed().as_millis() as u64);
        }
        self.output_chars += chars;
    }

    /// Final duration, or the time so far for a request that is still running.
    pub fn elapsed_ms(&self) -> u64 {
        self.duration_ms.unwrap_or_else(|| self.started.elapsed().as_millis() as u64)
    }

    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "time": self.at.to_rfc3339_opts(SecondsFormat::Millis, false),
            "status": self.status.as_str(),
            "http_status": self.http_status,
            "agent": self.agent,
            "model": self.model,
            "stream": self.stream,
            "client": self.client,
            "cwd": self.cwd,
            "session": self.session,
            "resumed": self.resumed,
            "prompt_chars": self.prompt_chars,
            "elapsed_ms": self.elapsed_ms(),
            "first_output_ms": self.first_output_ms,
            "output_chars": self.output_chars,
            "prompt_tokens": self.tokens.map(|t| t.0),
            "completion_tokens": self.tokens.map(|t| t.1),
            "error": self.error,
        })
    }
}

struct Inner {
    next_id: u64,
    entries: VecDeque<Entry>,
    capacity: usize,
    ok: u64,
    failed: u64,
    cancelled: u64,
}

pub struct ReqLog {
    inner: Mutex<Inner>,
    color: bool,
    boot: Instant,
}

impl ReqLog {
    pub fn new(capacity: usize) -> Arc<Self> {
        let color = std::io::stdout().is_terminal()
            && std::env::var_os("NO_COLOR").is_none()
            // legacy Windows consoles do not understand ANSI; Windows Terminal sets WT_SESSION
            && (!cfg!(windows) || std::env::var_os("WT_SESSION").is_some());
        Arc::new(ReqLog {
            inner: Mutex::new(Inner {
                next_id: 1,
                entries: VecDeque::new(),
                capacity: capacity.max(1),
                ok: 0,
                failed: 0,
                cancelled: 0,
            }),
            color,
            boot: Instant::now(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn start(self: &Arc<Self>, client: String) -> Tracker {
        let mut g = self.lock();
        let id = g.next_id;
        g.next_id += 1;
        if g.entries.len() >= g.capacity {
            g.entries.pop_front();
        }
        g.entries.push_back(Entry::new(id, client));
        Tracker { log: self.clone(), id, finished: false }
    }

    fn with<R>(&self, id: u64, f: impl FnOnce(&mut Entry) -> R) -> Option<R> {
        let mut g = self.lock();
        g.entries.iter_mut().rev().find(|e| e.id == id).map(f)
    }

    /// The newest `limit` entries (oldest first) matching `filter`, plus counters.
    pub fn snapshot(&self, limit: usize, filter: Option<Status>) -> (Vec<Entry>, Value) {
        let g = self.lock();
        let mut picked: Vec<Entry> = g
            .entries
            .iter()
            .rev()
            .filter(|e| filter.is_none_or(|s| e.status == s))
            .take(limit)
            .cloned()
            .collect();
        picked.reverse();
        let running = g.entries.iter().filter(|e| e.status == Status::Running).count();
        let summary = json!({
            "uptime_secs": self.boot.elapsed().as_secs(),
            "running": running,
            "ok": g.ok,
            "failed": g.failed,
            "cancelled": g.cancelled,
            "buffered": g.entries.len(),
            "capacity": g.capacity,
        });
        (picked, summary)
    }

    pub fn entries_json(entries: &[Entry]) -> Vec<Value> {
        entries.iter().rev().map(Entry::to_json).collect() // newest first
    }
}

/// Handle for one request. Dropping it without calling `finish` (client went away,
/// handler cancelled) records the request as cancelled.
pub struct Tracker {
    log: Arc<ReqLog>,
    id: u64,
    finished: bool,
}

impl Tracker {
    pub fn update(&self, f: impl FnOnce(&mut Entry)) {
        self.log.with(self.id, f);
    }

    /// Print the "request started" line.
    pub fn announce(&self) {
        if let Some(e) = self.log.with(self.id, |e| e.clone()) {
            println!("{}", format_line(&e, Local::now(), self.log.color));
        }
    }

    pub fn finish(mut self, status: Status, http_status: Option<u16>, error: Option<String>) {
        self.finished = true;
        self.complete(status, http_status, error);
    }

    fn complete(&self, status: Status, http_status: Option<u16>, error: Option<String>) {
        let entry = {
            let mut g = self.log.lock();
            match status {
                Status::Ok => g.ok += 1,
                Status::Failed => g.failed += 1,
                Status::Cancelled => g.cancelled += 1,
                Status::Running => {}
            }
            g.entries.iter_mut().rev().find(|e| e.id == self.id).map(|e| {
                e.status = status;
                e.http_status = http_status;
                e.error = error;
                e.duration_ms = Some(e.started.elapsed().as_millis() as u64);
                e.clone()
            })
        };
        if let Some(e) = entry {
            println!("{}", format_line(&e, Local::now(), self.log.color));
        }
    }
}

impl Drop for Tracker {
    fn drop(&mut self) {
        if !self.finished {
            self.complete(Status::Cancelled, None, Some("client disconnected".into()));
        }
    }
}

fn secs(ms: u64) -> String {
    format!("{:.2}s", ms as f64 / 1000.0)
}

/// One human-readable line; `stamp` is the time shown at the start of the line.
pub fn format_line(e: &Entry, stamp: DateTime<Local>, color: bool) -> String {
    let (tag, ansi) = match e.status {
        Status::Running => ("RUN   ", "36"),
        Status::Ok => ("OK    ", "32"),
        Status::Failed => ("FAIL  ", "31"),
        Status::Cancelled => ("CANCEL", "33"),
    };
    let mut s = format!("{} #{:<4} {}", stamp.format("%Y-%m-%d %H:%M:%S%.3f"), e.id, tag);
    if let Some(h) = e.http_status {
        s.push_str(&format!(" {h}"));
    }
    if e.agent.is_empty() {
        s.push_str(" -");
    } else {
        s.push_str(&format!(" {}/{}", e.agent, e.model));
    }
    s.push_str(if e.stream { " stream" } else { " once" });
    s.push_str(&format!(" {}", secs(e.elapsed_ms())));
    if let Some(f) = e.first_output_ms {
        s.push_str(&format!(" first={}", secs(f)));
    }
    if e.output_chars > 0 {
        s.push_str(&format!(" out={}ch", e.output_chars));
    }
    if let Some((p, c)) = e.tokens {
        s.push_str(&format!(" tok={p}/{c}"));
    }
    if let Some(sess) = &e.session {
        s.push_str(&format!(" session={sess}"));
    }
    if e.resumed {
        s.push_str(" resumed");
    }
    s.push_str(&format!(" from={}", e.client));
    if let Some(err) = &e.error {
        let one_line: String = err.split_whitespace().collect::<Vec<_>>().join(" ");
        let short: String = one_line.chars().take(300).collect();
        s.push_str(&format!(" | {short}"));
    }
    if color { format!("\x1b[{ansi}m{s}\x1b[0m") } else { s }
}
