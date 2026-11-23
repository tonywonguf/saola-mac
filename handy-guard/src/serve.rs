//! A small HTTP proxy between Handy and the cleanup model server (Ollama, OpenAI API).
//!
//! Handy sends the post-process request to this proxy. The proxy applies the exact custom-word
//! rules to the user message, sends the request on to the upstream server, and returns the
//! answer to Handy. Only `GET /v1/models` and `POST /v1/chat/completions` are forwarded; any
//! other path gets 404, and a request whose Host header is not localhost or 127.0.0.1 gets 403
//! (so a DNS-rebinding page cannot use the proxy to get around Ollama's own Host check).
//!
//! The proxy fails open: when the upstream is down, errors, or does not answer within the
//! total deadline, a chat request gets a valid chat completion whose content is the transcript
//! with the rules applied. A dictation is never lost because the model server is not there.
//!
//! Only plain HTTP/1.1 with Content-Length is handled, which is what Handy and Ollama use on
//! localhost. Each connection is one request (Connection: close). Request limits: 16 KiB of
//! request line and headers, 1 MiB of body, a total read deadline, 8 concurrent requests, and
//! no chunked request bodies (answer 411).

use crate::rules::{apply, Rule};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const MAX_HEAD: usize = 16 * 1024;
pub const MAX_BODY: usize = 1024 * 1024;
pub const MAX_CONCURRENT: usize = 8;
pub const MAX_UPSTREAM_BODY: usize = 8 * 1024 * 1024;
pub const LOG_MAX: u64 = 1024 * 1024;

pub struct Config {
    pub listen: String,
    pub upstream: String,
    pub rules: Vec<Rule>,
    pub log: Option<std::path::PathBuf>,
    /// Total time for the upstream part of one request (connect, send, answer). Default 8 s.
    pub deadline: Duration,
    /// Total time to read one request from the client. Default 10 s.
    pub read_timeout: Duration,
}

impl Config {
    pub fn new(
        listen: String,
        upstream: String,
        rules: Vec<Rule>,
        log: Option<std::path::PathBuf>,
    ) -> Config {
        Config {
            listen,
            upstream,
            rules,
            log,
            deadline: Duration::from_secs(8),
            read_timeout: Duration::from_secs(10),
        }
    }
}

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// A request that is refused with an HTTP status.
struct Refusal(u16, &'static str);

/// Reads from a stream, with every read limited by one total deadline.
struct DeadlineReader<'a> {
    s: &'a TcpStream,
    end: Instant,
}

impl Read for DeadlineReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        self.s.set_read_timeout(Some(left))?;
        (&*self.s).read(buf)
    }
}

fn io_refusal(e: std::io::Error) -> Refusal {
    match e.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
            Refusal(408, "request timeout")
        }
        _ => Refusal(400, "bad request"),
    }
}

/// Read one line of the head. `budget` is what is left of the head limit.
fn head_line<R: BufRead>(r: &mut R, budget: &mut usize) -> Result<String, Refusal> {
    let mut buf = Vec::new();
    let n = r
        .by_ref()
        .take(*budget as u64 + 1)
        .read_until(b'\n', &mut buf)
        .map_err(io_refusal)?;
    if n > *budget {
        return Err(Refusal(431, "request head too large"));
    }
    if n == 0 {
        return Err(Refusal(400, "bad request"));
    }
    *budget -= n;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn read_request(stream: &TcpStream, total: Duration) -> Result<Request, Refusal> {
    let mut r = BufReader::new(DeadlineReader {
        s: stream,
        end: Instant::now() + total,
    });
    let mut budget = MAX_HEAD;
    let line = head_line(&mut r, &mut budget)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    if method.is_empty() || path.is_empty() {
        return Err(Refusal(400, "bad request line"));
    }
    let mut headers = Vec::new();
    let mut len: Option<usize> = None;
    loop {
        let h = head_line(&mut r, &mut budget)?;
        if h == "\r\n" || h == "\n" {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            let (k, v) = (k.trim().to_string(), v.trim().to_string());
            if k.eq_ignore_ascii_case("content-length") {
                let n: usize = v.parse().map_err(|_| Refusal(400, "bad Content-Length"))?;
                if len.is_some_and(|old| old != n) {
                    return Err(Refusal(400, "conflicting Content-Length"));
                }
                len = Some(n);
            }
            if k.eq_ignore_ascii_case("transfer-encoding") {
                return Err(Refusal(
                    411,
                    "chunked request bodies are not supported; send Content-Length",
                ));
            }
            headers.push((k, v));
        }
    }
    let len = len.unwrap_or(0);
    if len > MAX_BODY {
        return Err(Refusal(413, "request body too large"));
    }
    let mut body = Vec::new();
    r.by_ref()
        .take(len as u64)
        .read_to_end(&mut body)
        .map_err(io_refusal)?;
    if body.len() != len {
        return Err(Refusal(400, "request body shorter than Content-Length"));
    }
    Ok(Request {
        method,
        path,
        headers,
        body,
    })
}

/// True when the Host header names this machine: localhost or 127.0.0.1, with an optional port.
pub fn host_allowed(host: &str) -> bool {
    let host = host.trim();
    let name = match host.rsplit_once(':') {
        Some((n, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => n,
        Some(_) => return false,
        None => host,
    };
    name.eq_ignore_ascii_case("localhost") || name == "127.0.0.1"
}

/// True for the two requests that are forwarded.
pub fn route_allowed(method: &str, path: &str) -> bool {
    let p = path.split(['?', '#']).next().unwrap_or("");
    matches!(
        (method, p),
        ("GET", "/v1/models") | ("POST", "/v1/chat/completions")
    )
}

/// The result of rewriting a request body.
pub struct Rewritten {
    pub body: Vec<u8>,
    pub replacements: usize,
    /// The spoken text (user message without the leading `[Styling: ...]` control lines).
    pub transcript: Option<String>,
    pub stream: bool,
    pub model: Option<String>,
}

/// The spoken text inside a cleanup prompt: the lines that are not `[...]` control lines.
pub fn transcript_of(content: &str) -> String {
    let t: Vec<&str> = content
        .lines()
        .filter(|l| !l.trim_start().starts_with('['))
        .collect();
    let t = t.join("\n");
    if t.trim().is_empty() {
        content.trim().to_string()
    } else {
        t.trim().to_string()
    }
}

/// Apply the rules to every user message whose content is a string, and cap the answer length
/// (`max_tokens`) when the request has no cap: about twice the transcript, so a model that
/// loops stops early. Returns the new body, the number of replacements and the transcript.
pub fn rewrite_body(body: &[u8], rules: &[Rule]) -> Rewritten {
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Rewritten {
            body: body.to_vec(),
            replacements: 0,
            transcript: None,
            stream: false,
            model: None,
        };
    };
    let mut total = 0;
    let mut transcript = None;
    if let Some(msgs) = v.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for m in msgs {
            if m.get("role").and_then(|r| r.as_str()) != Some("user") {
                continue;
            }
            if let Some(c) = m.get_mut("content") {
                if let Some(s) = c.as_str() {
                    let (t, n) = apply(s, rules);
                    total += n;
                    transcript = Some(transcript_of(&t));
                    *c = serde_json::Value::String(t);
                }
            }
        }
    }
    let stream = v.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    let model = v.get("model").and_then(|m| m.as_str()).map(str::to_string);
    if let (Some(t), Some(obj)) = (&transcript, v.as_object_mut()) {
        if !obj.contains_key("max_tokens") && !obj.contains_key("max_completion_tokens") {
            obj.insert(
                "max_tokens".into(),
                serde_json::json!(t.chars().count() / 2 + 64),
            );
        }
    }
    let body = serde_json::to_vec(&v).unwrap_or_else(|_| body.to_vec());
    Rewritten {
        body,
        replacements: total,
        transcript,
        stream,
        model,
    }
}

/// Why a cleanup answer looks broken, if it does. Checked on 200 saved cleanups: the 3 bad ones
/// (a loop of 42,000 characters and two added sentences) are all longer than 1.3 times the
/// transcript plus 20 characters; the longest good one is 1.1 times. An answer under half the
/// transcript (for a transcript over 40 characters) is a cut-off answer.
pub fn broken(answer: &str, transcript: &str) -> Option<&'static str> {
    let (a, t) = (
        answer.chars().count() as f64,
        transcript.chars().count() as f64,
    );
    if a > 1.3 * t + 20.0 {
        return Some("too-long");
    }
    let words: Vec<String> = answer.split_whitespace().map(crate::norm).collect();
    let mut run = 1;
    for i in 1..words.len() {
        run = if !words[i].is_empty() && words[i] == words[i - 1] {
            run + 1
        } else {
            1
        };
        if run >= 6 {
            return Some("repeat");
        }
    }
    if answer.trim().is_empty() && !transcript.trim().is_empty() {
        return Some("empty");
    }
    if t > 40.0 && a < 0.5 * t {
        return Some("too-short");
    }
    None
}

/// Check and fix a non-streamed chat answer. Applies the rules to the answer (the model can
/// undo them, for example "Taskhive" back to "task hive"), and falls back to the transcript
/// when the answer looks broken or was cut off (`finish_reason` "length"). Returns the new
/// body, the replacements and the fallback reason.
pub fn fix_answer(
    body: &[u8],
    transcript: &str,
    rules: &[Rule],
) -> Option<(Vec<u8>, usize, Option<&'static str>)> {
    let mut v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let answer = v
        .pointer("/choices/0/message/content")?
        .as_str()?
        .to_string();
    let truncated = v
        .pointer("/choices/0/finish_reason")
        .and_then(|f| f.as_str())
        == Some("length");
    let reason = if truncated {
        Some("truncated")
    } else {
        broken(&answer, transcript)
    };
    let base = if reason.is_some() {
        transcript.to_string()
    } else {
        answer
    };
    let (fixed, n) = apply(&base, rules);
    *v.pointer_mut("/choices/0/message/content")? = serde_json::Value::String(fixed);
    if reason.is_some() {
        if let Some(f) = v.pointer_mut("/choices/0/finish_reason") {
            *f = serde_json::json!("stop");
        }
    }
    Some((serde_json::to_vec(&v).ok()?, n, reason))
}

/// A valid chat completion whose content is the transcript (the fail-open answer).
pub fn fallback_completion(transcript: &str, model: Option<&str>) -> Vec<u8> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    serde_json::to_vec(&serde_json::json!({
        "id": format!("chatcmpl-guard-{now}"),
        "object": "chat.completion",
        "created": now,
        "model": model.unwrap_or("handy-guard"),
        "choices": [{"index": 0, "message": {"role": "assistant", "content": transcript}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}
    }))
    .unwrap_or_default()
}

fn decode_chunked(mut b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(pos) = b.windows(2).position(|w| w == b"\r\n") {
        let size = usize::from_str_radix(
            std::str::from_utf8(&b[..pos])
                .unwrap_or("0")
                .split(';')
                .next()
                .unwrap_or("0")
                .trim(),
            16,
        )
        .unwrap_or(0);
        if size == 0 || b.len() < pos + 2 + size {
            break;
        }
        out.extend_from_slice(&b[pos + 2..pos + 2 + size]);
        b = &b[(pos + 4 + size).min(b.len())..];
    }
    out
}

static LOG_LOCK: Mutex<()> = Mutex::new(());

/// Append one line to the request log. At 1 MiB the log moves to `requests.log.1` (one old
/// file is kept) and a new log starts.
fn log_line(cfg: &Config, line: &str) {
    let Some(log) = &cfg.log else { return };
    let _guard = LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if std::fs::metadata(log)
        .map(|m| m.len() >= LOG_MAX)
        .unwrap_or(false)
    {
        let mut old = log.clone().into_os_string();
        old.push(".1");
        let _ = std::fs::rename(log, old);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "{now}\t{line}");
    }
}

fn reason_phrase(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        502 => "Bad Gateway",
        _ => "Service Unavailable",
    }
}

fn respond(client: &mut TcpStream, code: u16, body: &[u8]) -> std::io::Result<()> {
    let head = format!("HTTP/1.1 {code} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", reason_phrase(code), body.len());
    client.write_all(head.as_bytes())?;
    client.write_all(body)?;
    client.flush()
}

fn respond_error(client: &mut TcpStream, code: u16, msg: &str) -> std::io::Result<()> {
    respond(
        client,
        code,
        &serde_json::to_vec(&serde_json::json!({"error": {"message": msg, "type": "handy-guard"}}))
            .unwrap_or_default(),
    )
}

fn connect(upstream: &str, deadline: Instant) -> std::io::Result<TcpStream> {
    let mut last = std::io::Error::new(std::io::ErrorKind::NotFound, "no upstream address");
    for addr in upstream.to_socket_addrs()? {
        let left = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_secs(1));
        if left.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        match TcpStream::connect_timeout(&addr, left) {
            Ok(s) => return Ok(s),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Send the request upstream and read the whole answer, all inside the deadline.
fn fetch(cfg: &Config, req: &Request, body: &[u8], deadline: Instant) -> std::io::Result<Vec<u8>> {
    let mut up = connect(&cfg.upstream, deadline)?;
    send(&mut up, cfg, req, body, deadline)?;
    let mut raw = Vec::new();
    let mut r = DeadlineReader {
        s: &up,
        end: deadline,
    };
    r.by_ref()
        .take(MAX_UPSTREAM_BODY as u64 + 1)
        .read_to_end(&mut raw)?;
    if raw.len() > MAX_UPSTREAM_BODY {
        return Err(std::io::Error::other("upstream answer too large"));
    }
    Ok(raw)
}

fn send(
    up: &mut TcpStream,
    cfg: &Config,
    req: &Request,
    body: &[u8],
    deadline: Instant,
) -> std::io::Result<()> {
    up.set_write_timeout(Some(
        deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1)),
    ))?;
    let mut head = format!("{} {} HTTP/1.1\r\n", req.method, req.path);
    for (k, v) in &req.headers {
        // Host is set to the upstream address. Origin and Referer are not passed on.
        let skip = [
            "content-length",
            "connection",
            "host",
            "origin",
            "referer",
            "transfer-encoding",
        ];
        if skip.iter().any(|s| k.eq_ignore_ascii_case(s)) {
            continue;
        }
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!(
        "Host: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        cfg.upstream,
        body.len()
    ));
    up.write_all(head.as_bytes())?;
    up.write_all(body)?;
    up.flush()
}

/// The upstream failed: answer with the transcript when there is one, else with a 502.
fn fail_open(
    client: &mut TcpStream,
    cfg: &Config,
    req: &Request,
    rw: &Rewritten,
    why: &str,
) -> std::io::Result<()> {
    match &rw.transcript {
        Some(t) => {
            log_line(
                cfg,
                &format!(
                    "{} {}\treplacements={}\tfallback=upstream-{why}",
                    req.method, req.path, rw.replacements
                ),
            );
            respond(client, 200, &fallback_completion(t, rw.model.as_deref()))
        }
        None => {
            log_line(
                cfg,
                &format!("{} {}\terror=upstream-{why}", req.method, req.path),
            );
            respond_error(client, 502, "upstream unavailable")
        }
    }
}

fn short_err(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => "timeout".into(),
        std::io::ErrorKind::ConnectionRefused => "refused".into(),
        k => format!("{k:?}").to_lowercase(),
    }
}

fn handle(mut client: TcpStream, cfg: &Config) -> std::io::Result<()> {
    client.set_write_timeout(Some(Duration::from_secs(10)))?;
    let req = match read_request(&client, cfg.read_timeout) {
        Ok(r) => r,
        Err(Refusal(code, msg)) => {
            log_line(cfg, &format!("refused\t{code}\t{msg}"));
            return respond_error(&mut client, code, msg);
        }
    };
    let host_ok = req
        .headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("host") && host_allowed(v));
    if !host_ok {
        log_line(cfg, "refused\t403\thost");
        return respond_error(&mut client, 403, "Host must be localhost or 127.0.0.1");
    }
    if !route_allowed(&req.method, &req.path) {
        log_line(
            cfg,
            &format!(
                "refused\t404\t{} {}",
                req.method,
                req.path.chars().take(80).collect::<String>()
            ),
        );
        return respond_error(&mut client, 404, "not found");
    }
    let deadline = Instant::now() + cfg.deadline;
    let rw = if req.method == "POST" {
        rewrite_body(&req.body, &cfg.rules)
    } else {
        Rewritten {
            body: req.body.clone(),
            replacements: 0,
            transcript: None,
            stream: false,
            model: None,
        }
    };

    // A non-streamed chat request with a transcript: read the whole answer, check it, fix it.
    if let Some(transcript) = rw.transcript.clone().filter(|_| !rw.stream) {
        let raw = match fetch(cfg, &req, &rw.body, deadline) {
            Ok(raw) => raw,
            Err(e) => return fail_open(&mut client, cfg, &req, &rw, &short_err(&e)),
        };
        let fixed = raw.windows(4).position(|w| w == b"\r\n\r\n").map(|p| {
            let head = String::from_utf8_lossy(&raw[..p]).to_string();
            let mut body = raw[p + 4..].to_vec();
            if head
                .to_ascii_lowercase()
                .contains("transfer-encoding: chunked")
            {
                body = decode_chunked(&body);
            }
            let status = head
                .lines()
                .next()
                .unwrap_or("")
                .split_whitespace()
                .nth(1)
                .unwrap_or("")
                .to_string();
            if status != "200" {
                return Err(format!("status-{status}"));
            }
            fix_answer(&body, &transcript, &cfg.rules).ok_or_else(|| "unparsable".to_string())
        });
        return match fixed {
            Some(Ok((b, n, reason))) => {
                log_line(
                    cfg,
                    &format!(
                        "{} {}\treplacements={}\tanswer_replacements={n}\tfallback={}",
                        req.method,
                        req.path,
                        rw.replacements,
                        reason.unwrap_or("none")
                    ),
                );
                respond(&mut client, 200, &b)
            }
            Some(Err(why)) => fail_open(&mut client, cfg, &req, &rw, &why),
            None => fail_open(&mut client, cfg, &req, &rw, "no-http-answer"),
        };
    }

    // A streamed answer, or a request without a transcript, is relayed as it comes.
    let mut up = match connect(&cfg.upstream, deadline)
        .and_then(|mut up| send(&mut up, cfg, &req, &rw.body, deadline).map(|_| up))
    {
        Ok(up) => up,
        Err(e) => return fail_open(&mut client, cfg, &req, &rw, &short_err(&e)),
    };
    log_line(
        cfg,
        &format!(
            "{} {}\treplacements={}",
            req.method, req.path, rw.replacements
        ),
    );
    let mut buf = [0u8; 16384];
    let mut started = false;
    loop {
        // The first bytes must arrive inside the deadline; a running stream may take longer.
        let wait = if started || rw.stream {
            Duration::from_secs(300)
        } else {
            deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(1))
        };
        up.set_read_timeout(Some(wait))?;
        match up.read(&mut buf) {
            Ok(0) => break,
            Ok(k) => {
                started = true;
                client.write_all(&buf[..k])?;
            }
            Err(e) if !started => return fail_open(&mut client, cfg, &req, &rw, &short_err(&e)),
            Err(e) => return Err(e),
        }
    }
    if !started {
        return fail_open(&mut client, cfg, &req, &rw, "empty-answer");
    }
    client.flush()
}

struct Slot(Arc<AtomicUsize>);
impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Serve on an already bound listener. Never returns.
pub fn serve_on(listener: TcpListener, cfg: Config) -> std::io::Result<()> {
    let cfg = Arc::new(cfg);
    let active = Arc::new(AtomicUsize::new(0));
    for conn in listener.incoming() {
        let Ok(mut conn) = conn else { continue };
        if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONCURRENT {
            active.fetch_sub(1, Ordering::SeqCst);
            let _ = conn.set_write_timeout(Some(Duration::from_secs(1)));
            let _ = respond_error(&mut conn, 503, "too many requests");
            continue;
        }
        let (cfg, slot) = (cfg.clone(), Slot(active.clone()));
        std::thread::spawn(move || {
            let _slot = slot;
            if let Err(e) = handle(conn, &cfg) {
                eprintln!("handy-guard: request failed: {e}");
            }
        });
    }
    Ok(())
}

pub fn run(cfg: Config) -> std::io::Result<()> {
    let listener = TcpListener::bind(&cfg.listen)?;
    eprintln!(
        "handy-guard: listening on {} -> {} ({} rules)",
        cfg.listen,
        cfg.upstream,
        cfg.rules.len()
    );
    serve_on(listener, cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{parse_rules, DEFAULT_RULES};

    #[test]
    fn rewrites_only_user_messages() {
        let body = br#"{"model":"s1-mini","messages":[{"role":"system","content":"task hive"},{"role":"user","content":"[Context: general]\nadd a task hive"}],"reasoning_effort":"none"}"#;
        let rw = rewrite_body(body, &parse_rules(DEFAULT_RULES));
        assert_eq!(rw.replacements, 1);
        assert_eq!(rw.transcript.as_deref(), Some("add a Taskhive"));
        let v: serde_json::Value = serde_json::from_slice(&rw.body).unwrap();
        assert_eq!(v["messages"][0]["content"], "task hive");
        assert_eq!(
            v["messages"][1]["content"],
            "[Context: general]\nadd a Taskhive"
        );
        assert_eq!(v["reasoning_effort"], "none");
        assert_eq!(v["max_tokens"], 14 / 2 + 64);
    }

    #[test]
    fn leaves_other_bodies_unchanged() {
        let body = b"not json";
        assert_eq!(rewrite_body(body, &[]).body, body.to_vec());
    }

    fn answer(text: &str) -> Vec<u8> {
        serde_json::to_vec(
            &serde_json::json!({"choices":[{"message":{"role":"assistant","content":text}}]}),
        )
        .unwrap()
    }

    fn content(body: &[u8]) -> String {
        let v: serde_json::Value = serde_json::from_slice(body).unwrap();
        v["choices"][0]["message"]["content"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn falls_back_on_a_loop() {
        let t = "there should be deadlines, tests and quizzes";
        let looped = format!("There should be deadlines, {}", "tests, ".repeat(40));
        let (b, _, reason) = fix_answer(&answer(&looped), t, &[]).unwrap();
        assert_eq!(reason, Some("too-long"));
        assert_eq!(content(&b), t);
        assert_eq!(
            broken(
                "tests tests tests tests tests tests",
                "tests tests tests tests tests tests and more words here"
            ),
            Some("repeat")
        );
    }

    #[test]
    fn falls_back_on_added_sentences() {
        let t = "What are some cool things I can do with developer settings with my T V and what should I change?";
        let a = "What are some cool things I can do with developer settings with my TV and what should I change? I can do a lot of things with TV settings. What should I change?";
        assert_eq!(fix_answer(&answer(a), t, &[]).unwrap().2, Some("too-long"));
    }

    #[test]
    fn keeps_a_good_answer_and_reapplies_rules() {
        let rules = parse_rules(DEFAULT_RULES);
        let t = "so make a Taskhive for this task";
        let (b, n, reason) =
            fix_answer(&answer("So make a task hive for this task."), t, &rules).unwrap();
        assert_eq!(reason, None);
        assert_eq!(n, 1);
        assert_eq!(content(&b), "So make a Taskhive for this task.");
    }

    #[test]
    fn reads_chunked_bodies() {
        assert_eq!(
            decode_chunked(b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"),
            b"hello world".to_vec()
        );
    }
}
