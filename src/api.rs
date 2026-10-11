use crate::{
    config::{Config, Overrides},
    reqlog::{self, ReqLog, Status, Tracker},
    rules::Ev,
    runner::{self, Msg},
    sessions::Sessions,
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{ConnectInfo, DefaultBodyLimit, Query, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::{
        IntoResponse, Response,
        sse::{Event as Sse, KeepAlive, Sse as SseResponse},
    },
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    convert::Infallible,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc;

const SESSION_HEADER: &str = "x-session-id";

struct Loaded {
    cfg: Arc<Config>,
    mtime: Option<SystemTime>,
}

pub struct AppState {
    loaded: RwLock<Loaded>,
    config_path: Option<PathBuf>,
    overrides: Overrides,
    sessions: Sessions,
    log: Arc<ReqLog>,
}

impl AppState {
    pub fn new(
        cfg: Config,
        config_path: Option<PathBuf>,
        overrides: Overrides,
    ) -> Arc<Self> {
        let mtime = config_path.as_deref().and_then(mtime_of);
        let log = ReqLog::new(cfg.server.log_capacity);
        Arc::new(Self {
            loaded: RwLock::new(Loaded { cfg: Arc::new(cfg), mtime }),
            config_path,
            overrides,
            sessions: Sessions::default(),
            log,
        })
    }

    /// Current config; re-reads the file first if it changed on disk.
    /// A broken edit keeps the previous config running.
    fn config(&self) -> Arc<Config> {
        let now = self.config_path.as_deref().and_then(mtime_of);
        {
            let loaded = self.loaded.read().unwrap();
            if now == loaded.mtime {
                return loaded.cfg.clone();
            }
        }
        let mut loaded = self.loaded.write().unwrap();
        if now != loaded.mtime {
            loaded.mtime = now; // don't retry a broken file on every request
            match Config::load(self.config_path.as_deref(), &self.overrides) {
                Ok(cfg) => {
                    eprintln!("config reloaded ({} agents)", cfg.agents.len());
                    loaded.cfg = Arc::new(cfg);
                }
                Err(e) => eprintln!("config reload failed, keeping previous config: {e}"),
            }
        }
        loaded.cfg.clone()
    }
}

fn mtime_of(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(info))
        .route("/v1", get(info))
        .route("/v1/", get(info))
        .route("/health", get(|| async { "ok" }))
        .route("/v1/models", get(models))
        .route("/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .route("/chat/completions", post(chat))
        .route("/requests", get(requests))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(32 * 1024 * 1024))
        .with_state(state)
}

/// Public landing answer for `/`, `/v1` and `/v1/` (what a browser sees at the base URL).
/// Says nothing sensitive: no version, no agents, no models.
async fn info() -> Json<Value> {
    Json(json!({
        "service": "tricky-agent",
        "status": "ok",
        "base_url": "/v1",
        "endpoints": ["GET /v1/models", "POST /v1/chat/completions", "GET /requests"],
        "auth": "send the secret key as: Authorization: Bearer <key>",
    }))
}

async fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "invalid_request_error", "unknown path; see GET /v1")
}

// ---------------------------------------------------------------- helpers

struct ApiError {
    status: StatusCode,
    kind: &'static str,
    message: String,
    session: Option<String>,
}

impl ApiError {
    fn new(status: StatusCode, kind: &'static str, message: impl Into<String>) -> Self {
        Self { status, kind, message: message.into(), session: None }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request_error", message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({"error": {"message": self.message, "type": self.kind, "param": null, "code": null}});
        with_session_header((self.status, Json(body)).into_response(), &self.session)
    }
}

fn authorize(headers: &HeaderMap, cfg: &Config) -> Result<(), ApiError> {
    let key = &cfg.server.api_key;
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    // an empty key never authorises anything (fail closed); compare without early exit
    let ok = !key.is_empty()
        && given.len() == key.len()
        && given.bytes().zip(key.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0;
    if ok {
        Ok(())
    } else {
        Err(ApiError::new(StatusCode::UNAUTHORIZED, "invalid_request_error", "invalid or missing API key"))
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn with_session_header(mut resp: Response, session: &Option<String>) -> Response {
    if let Some(v) = session.as_deref().and_then(|s| HeaderValue::from_str(s).ok()) {
        resp.headers_mut().insert(HeaderName::from_static(SESSION_HEADER), v);
    }
    resp
}

/// Real client address when a tunnel/proxy forwards it, else the TCP peer.
fn client_label(headers: &HeaderMap, peer: SocketAddr) -> String {
    let forwarded = headers
        .get("cf-connecting-ip")
        .or_else(|| headers.get("x-forwarded-for"))
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|v| !v.is_empty());
    match forwarded {
        Some(f) => format!("{f} (via {})", peer.ip()),
        None => peer.ip().to_string(),
    }
}

// ---------------------------------------------------------------- /v1/models

async fn models(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    let cfg = st.config();
    authorize(&headers, &cfg)?;
    let mut ids = Vec::new();
    for (name, agent) in &cfg.agents {
        ids.push(format!("{name}/auto"));
        ids.extend(agent.models.iter().filter(|m| *m != "auto").map(|m| format!("{name}/{m}")));
    }
    let data: Vec<Value> = ids
        .iter()
        .map(|id| json!({"id": id, "object": "model", "created": 0, "owned_by": "tricky-agent"}))
        .collect();
    Ok(Json(json!({"object": "list", "data": data})))
}

// ---------------------------------------------------------------- /requests

#[derive(Deserialize)]
struct RequestsQuery {
    limit: Option<usize>,
    status: Option<String>,
    format: Option<String>,
}

async fn requests(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<RequestsQuery>,
) -> Result<Response, ApiError> {
    let cfg = st.config();
    authorize(&headers, &cfg)?;
    let filter = match q.status.as_deref().filter(|s| !s.is_empty()) {
        None => None,
        Some(s) => Some(
            Status::parse(s)
                .ok_or_else(|| ApiError::bad_request("status must be one of: running, ok, failed, cancelled"))?,
        ),
    };
    let limit = q.limit.unwrap_or(50).clamp(1, 10_000);
    let (entries, summary) = st.log.snapshot(limit, filter);

    if q.format.as_deref() == Some("text") {
        let mut out = format!(
            "running={} ok={} failed={} cancelled={} uptime={}s\n",
            summary["running"], summary["ok"], summary["failed"], summary["cancelled"], summary["uptime_secs"]
        );
        for e in &entries {
            out.push_str(&reqlog::format_line(e, e.at, false));
            out.push('\n');
        }
        return Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], out).into_response());
    }
    Ok(Json(json!({"object": "list", "summary": summary, "data": ReqLog::entries_json(&entries)})).into_response())
}

// ---------------------------------------------------------------- chat

#[derive(Deserialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    #[serde(default)]
    stream: bool,
    stream_options: Option<StreamOptions>,
    // tricky-agent extensions
    session_id: Option<String>,
    user: Option<String>,
    new_session: Option<bool>,
    yolo: Option<bool>,
    cwd: Option<String>,
    timeout_secs: Option<u64>,
}

#[derive(Deserialize)]
struct StreamOptions {
    include_usage: Option<bool>,
}

#[derive(Deserialize)]
struct Message {
    role: String,
    #[serde(default)]
    content: Value,
}

fn content_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn label(role: &str) -> &'static str {
    match role {
        "system" | "developer" => "System",
        "assistant" => "Assistant",
        "tool" => "Tool",
        _ => "User",
    }
}

/// Resuming: the agent already remembers everything, so send only what is new
/// (the messages after the last assistant turn).
/// Otherwise: send the whole conversation as a transcript.
fn build_prompt(messages: &[Message], resuming: bool) -> Result<String, ApiError> {
    if messages.is_empty() {
        return Err(ApiError::bad_request("`messages` must not be empty"));
    }
    let slice: &[Message] = if resuming {
        let start = messages.iter().rposition(|m| m.role == "assistant").map_or(0, |i| i + 1);
        if start >= messages.len() { &messages[messages.len() - 1..] } else { &messages[start..] }
    } else {
        messages
    };
    if slice.len() == 1 {
        return Ok(content_text(&slice[0].content));
    }
    let mut out = String::new();
    if !resuming {
        out.push_str("Below is the conversation so far. Reply as the assistant to the final message.\n\n");
    }
    for m in slice {
        out.push_str(&format!("{}: {}\n\n", label(&m.role), content_text(&m.content)));
    }
    Ok(out.trim_end().to_string())
}

/// Default working folder (created on demand) or a requested one inside `workspace_roots`.
/// A relative `cwd` is taken relative to the default folder.
fn resolve_cwd(requested: Option<&str>, cfg: &Config) -> Result<PathBuf, ApiError> {
    let default = &cfg.server.default_cwd;
    std::fs::create_dir_all(default).map_err(|e| {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "server_error", format!("cannot create {}: {e}", default.display()))
    })?;
    let Some(req) = requested.filter(|r| !r.is_empty()) else {
        return Ok(default.clone());
    };
    let wanted = if Path::new(req).is_absolute() { PathBuf::from(req) } else { default.join(req) };
    let dir = std::fs::canonicalize(&wanted).map_err(|e| ApiError::bad_request(format!("cwd {req:?}: {e}")))?;
    let roots: Vec<PathBuf> = if cfg.server.workspace_roots.is_empty() {
        vec![default.clone()]
    } else {
        cfg.server.workspace_roots.clone()
    };
    let allowed = roots
        .iter()
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .any(|r| dir.starts_with(r));
    if allowed {
        Ok(dir)
    } else {
        Err(ApiError::bad_request(format!("cwd {req:?} is outside the allowed folders (workspace_roots)")))
    }
}

async fn chat(
    State(st): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut tracker = Some(st.log.start(client_label(&headers, peer)));
    match chat_inner(&st, &mut tracker, &headers, body).await {
        Ok(resp) => {
            // a streamed response keeps its tracker and finishes it when the stream ends
            if let Some(t) = tracker.take() {
                t.finish(Status::Ok, Some(resp.status().as_u16()), None);
            }
            resp
        }
        Err(e) => {
            if let Some(t) = tracker.take() {
                t.finish(Status::Failed, Some(e.status.as_u16()), Some(e.message.clone()));
            }
            e.into_response()
        }
    }
}

async fn chat_inner(
    st: &Arc<AppState>,
    tracker: &mut Option<Tracker>,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let cfg = st.config();
    authorize(headers, &cfg)?;
    let req: ChatRequest =
        serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(format!("bad request body: {e}")))?;

    // model = "<agent>/<model>"; "<agent>", "<agent>/auto" and "<agent>/" use the agent's default
    let (agent_name, agent_model) = match req.model.split_once('/') {
        Some((a, m)) => (a.to_string(), Some(m.to_string()).filter(|m| !m.is_empty() && m != "auto")),
        None => (req.model.clone(), None),
    };
    let Some(agent) = cfg.agents.get(&agent_name) else {
        let known = cfg.agents.keys().map(|a| format!("{a}/auto")).collect::<Vec<_>>().join(", ");
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            format!("unknown model {:?}: use \"<agent>/<model>\", for example {known}", req.model),
        ));
    };

    let track = tracker.as_ref().expect("tracker lives until the response is built");
    track.update(|e| {
        e.agent = agent_name.clone();
        e.model = agent_model.clone().unwrap_or_else(|| "auto".to_string());
        e.stream = req.stream;
    });

    // ---- session: body.session_id > X-Session-Id header > user
    let requested_id = req
        .session_id
        .clone()
        .or_else(|| headers.get(SESSION_HEADER).and_then(|v| v.to_str().ok()).map(str::to_string))
        .or_else(|| req.user.clone())
        .filter(|s| !s.is_empty());

    let ttl = Duration::from_secs(cfg.server.session_ttl_secs);
    let (slot, custom_id) = match &requested_id {
        Some(id) => {
            // "<agent>:<native id>" is self-describing, no map needed
            let native_form = id.split_once(':').filter(|(a, _)| cfg.agents.contains_key(*a));
            match native_form {
                Some((a, native)) => {
                    if a != agent_name {
                        return Err(ApiError::bad_request(format!(
                            "session {id:?} belongs to agent {a:?}, but the request uses agent {agent_name:?}"
                        )));
                    }
                    (st.sessions.slot(&agent_name, id, Some(native.to_string()), ttl), None)
                }
                None => (st.sessions.slot(&agent_name, id, None, ttl), Some(id.clone())),
            }
        }
        None => (Arc::new(tokio::sync::Mutex::new(None)), None),
    };

    let cwd = resolve_cwd(req.cwd.as_deref(), &cfg)?;
    track.update(|e| {
        e.cwd = cwd.display().to_string();
        e.session = requested_id.clone();
    });
    track.announce();

    // one run at a time per session
    let mut guard = slot.lock_owned().await;
    if req.new_session == Some(true) {
        *guard = None;
    }
    let native = guard.clone().filter(|_| !agent.resume_args.is_empty());
    let prompt = build_prompt(&req.messages, native.is_some())?;
    track.update(|e| {
        e.prompt_chars = prompt.chars().count();
        e.resumed = native.is_some();
    });

    let timeout = Duration::from_secs(
        req.timeout_secs.map_or(cfg.server.timeout_secs, |t| t.min(cfg.server.timeout_secs)).max(1),
    );
    let spec = runner::build_spec(
        agent,
        &prompt,
        native.as_deref(),
        req.yolo.unwrap_or(cfg.server.default_yolo),
        agent_model.as_deref(),
        cwd,
        timeout,
    );

    // run the agent in its own task; it keeps the session lock until it is done,
    // and learns the agent's session id from the first event
    let (tx, mut rx) = mpsc::channel::<Msg>(64);
    tokio::spawn(async move {
        let (itx, mut irx) = mpsc::channel::<Msg>(64);
        let runner = tokio::spawn(runner::run(spec, itx));
        while let Some(msg) = irx.recv().await {
            if let Msg::Event(Ev::Session(id)) = &msg {
                *guard = Some(id.clone());
            }
            if tx.send(msg).await.is_err() {
                break; // client went away: dropping irx makes the runner kill the child
            }
        }
        drop(irx);
        let _ = runner.await;
    });

    // wait for the agent's session id (its first event) so it can go in the headers
    let mut head: VecDeque<Msg> = VecDeque::new();
    let mut native_id: Option<String> = None;
    while let Some(msg) = rx.recv().await {
        let stop = match &msg {
            Msg::Event(Ev::Session(id)) => {
                native_id = Some(id.clone());
                true
            }
            Msg::Event(Ev::Text(_)) | Msg::Done | Msg::Error(_) => true,
            Msg::Event(Ev::Usage { .. }) => false,
        };
        if !matches!(&msg, Msg::Event(Ev::Session(_))) {
            head.push_back(msg);
        }
        if stop {
            break;
        }
    }
    let session_out = custom_id.or_else(|| native_id.or(native).map(|n| format!("{agent_name}:{n}")));
    track.update(|e| e.session = session_out.clone());

    // an error before any text becomes a normal HTTP error, in both modes
    if let Some(Msg::Error(e)) = head.front() {
        let mut err = ApiError::new(StatusCode::BAD_GATEWAY, "agent_error", e.clone());
        err.session = session_out;
        return Err(err);
    }

    let id = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());
    let created = now_secs();
    let model_label = req.model.clone();

    if req.stream {
        let include_usage = req.stream_options.and_then(|o| o.include_usage).unwrap_or(false);
        let sse_session = session_out.clone();
        let tracker = tracker.take().expect("tracker present");
        let stream = async_stream::stream! {
            let chunk = |delta: Value, finish: Option<&str>| {
                json!({
                    "id": id, "object": "chat.completion.chunk", "created": created, "model": model_label,
                    "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
                    "session_id": sse_session,
                })
                .to_string()
            };
            yield Ok::<_, Infallible>(Sse::default().data(chunk(json!({"role": "assistant", "content": ""}), None)));

            let mut usage: Option<(u64, u64)> = None;
            let mut failure: Option<String> = None;
            loop {
                let msg = match head.pop_front() {
                    Some(m) => m,
                    None => match rx.recv().await {
                        Some(m) => m,
                        None => break,
                    },
                };
                match msg {
                    Msg::Event(Ev::Text(t)) => {
                        let n = t.chars().count();
                        tracker.update(|e| e.mark_output(n));
                        yield Ok(Sse::default().data(chunk(json!({"content": t}), None)));
                    }
                    Msg::Event(Ev::Usage { prompt, completion }) => usage = Some((prompt, completion)),
                    Msg::Event(Ev::Session(_)) => {}
                    Msg::Error(e) => {
                        let err = json!({"error": {"message": e, "type": "agent_error", "param": null, "code": null}});
                        failure = Some(e);
                        yield Ok(Sse::default().data(err.to_string()));
                        break;
                    }
                    Msg::Done => break,
                }
            }
            if failure.is_none() {
                yield Ok(Sse::default().data(chunk(json!({}), Some("stop"))));
                if include_usage {
                    let (p, c) = usage.unwrap_or((0, 0));
                    let u = json!({
                        "id": id, "object": "chat.completion.chunk", "created": created, "model": model_label,
                        "choices": [], "session_id": sse_session,
                        "usage": {"prompt_tokens": p, "completion_tokens": c, "total_tokens": p + c},
                    });
                    yield Ok(Sse::default().data(u.to_string()));
                }
            }
            tracker.update(|e| e.tokens = usage);
            match failure {
                Some(msg) => tracker.finish(Status::Failed, Some(200), Some(msg)),
                None => tracker.finish(Status::Ok, Some(200), None),
            }
            yield Ok(Sse::default().data("[DONE]"));
        };
        let resp = SseResponse::new(stream).keep_alive(KeepAlive::default()).into_response();
        return Ok(with_session_header(resp, &session_out));
    }

    // non-streaming: collect everything
    let mut text = String::new();
    let mut usage = (0u64, 0u64);
    let mut failure: Option<String> = None;
    loop {
        let msg = match head.pop_front() {
            Some(m) => m,
            None => match rx.recv().await {
                Some(m) => m,
                None => break,
            },
        };
        match msg {
            Msg::Event(Ev::Text(t)) => {
                let n = t.chars().count();
                track.update(|e| e.mark_output(n));
                text.push_str(&t);
            }
            Msg::Event(Ev::Usage { prompt, completion }) => usage = (prompt, completion),
            Msg::Event(Ev::Session(_)) => {}
            Msg::Error(e) => {
                failure = Some(e);
                break;
            }
            Msg::Done => break,
        }
    }
    track.update(|e| e.tokens = Some(usage));
    if let Some(e) = failure {
        let mut err = ApiError::new(StatusCode::BAD_GATEWAY, "agent_error", e);
        err.session = session_out;
        return Err(err);
    }
    let body = json!({
        "id": id, "object": "chat.completion", "created": created, "model": model_label,
        "choices": [{"index": 0, "message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": usage.0, "completion_tokens": usage.1, "total_tokens": usage.0 + usage.1},
        "session_id": session_out,
    });
    Ok(with_session_header(Json(body).into_response(), &session_out))
}
