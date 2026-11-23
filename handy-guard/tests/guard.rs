//! Integration tests for the proxy: fail-open, request limits, Host and path checks,
//! truncated answers and log rotation. A fake upstream is a std TcpListener thread.

use handy_guard::rules::{parse_rules, DEFAULT_RULES};
use handy_guard::serve::{serve_on, Config};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Start the proxy on a free port. Returns its address.
fn guard(upstream: &str, tweak: impl FnOnce(&mut Config)) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let mut cfg = Config::new(
        addr.clone(),
        upstream.to_string(),
        parse_rules(DEFAULT_RULES),
        None,
    );
    cfg.deadline = Duration::from_millis(1500);
    cfg.read_timeout = Duration::from_millis(1500);
    tweak(&mut cfg);
    std::thread::spawn(move || serve_on(l, cfg));
    addr
}

/// An address where nothing listens.
fn closed_port() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().to_string()
}

/// Read one HTTP request (head and Content-Length body) as text.
fn read_http(s: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut b = [0u8; 4096];
    loop {
        let n = s.read(&mut b).unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&b[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..p]).to_ascii_lowercase();
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            if buf.len() >= p + 4 + len {
                break;
            }
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// A fake upstream. The handler gets each connection and the request text.
fn fake(handler: impl Fn(&mut TcpStream, String) + Send + Sync + 'static) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let handler = std::sync::Arc::new(handler);
    std::thread::spawn(move || {
        for c in l.incoming() {
            let Ok(mut c) = c else { continue };
            let h = handler.clone();
            std::thread::spawn(move || {
                let req = read_http(&mut c);
                h(&mut c, req);
            });
        }
    });
    addr
}

fn json_reply(s: &mut TcpStream, status: &str, body: &str) {
    let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
}

fn completion(text: &str, finish: &str) -> String {
    serde_json::json!({"choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":finish}]}).to_string()
}

/// Send raw bytes, read the whole answer. Returns (status code, body).
fn raw(addr: &str, req: &[u8]) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let _ = s.write_all(req);
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    parse(&out)
}

fn parse(out: &[u8]) -> (u16, String) {
    let text = String::from_utf8_lossy(out).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map(|x| x.1.to_string())
        .unwrap_or_default();
    (status, body)
}

fn request(method: &str, path: &str, host: &str, body: &str) -> Vec<u8> {
    format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes()
}

const PROMPT: &str = "[Context: general]\nadd a task hive for this";
const TRANSCRIPT: &str = "add a Taskhive for this";

fn chat_body() -> String {
    serde_json::json!({"model":"s1-mini","messages":[{"role":"system","content":"clean up"},{"role":"user","content":PROMPT}]}).to_string()
}

fn chat(addr: &str) -> (u16, String) {
    raw(
        addr,
        &request("POST", "/v1/chat/completions", addr, &chat_body()),
    )
}

fn content_of(body: &str) -> String {
    let v: serde_json::Value =
        serde_json::from_str(body).unwrap_or_else(|e| panic!("not JSON ({e}): {body}"));
    v["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .to_string()
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("handy-guard-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

// G1: fail open

#[test]
fn fail_open_when_the_upstream_port_is_closed() {
    let g = guard(&closed_port(), |_| {});
    let t = Instant::now();
    let (status, body) = chat(&g);
    assert_eq!(status, 200);
    assert_eq!(content_of(&body), TRANSCRIPT);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["object"], "chat.completion");
    assert_eq!(v["model"], "s1-mini");
    assert_eq!(v["choices"][0]["finish_reason"], "stop");
    assert!(t.elapsed() < Duration::from_secs(3));
}

#[test]
fn fail_open_when_the_upstream_never_answers() {
    let (tx, rx) = mpsc::channel::<()>();
    let tx = std::sync::Mutex::new(tx);
    let up = fake(move |_s, _req| {
        // Hold the connection open and say nothing.
        std::thread::sleep(Duration::from_secs(5));
        let _ = tx.lock().unwrap().send(());
    });
    let g = guard(&up, |c| c.deadline = Duration::from_millis(400));
    let t = Instant::now();
    let (status, body) = chat(&g);
    assert_eq!(status, 200);
    assert_eq!(content_of(&body), TRANSCRIPT);
    let took = t.elapsed();
    assert!(
        took >= Duration::from_millis(350) && took < Duration::from_secs(3),
        "{took:?}"
    );
    drop(rx);
}

#[test]
fn fail_open_when_the_upstream_answers_slowly_in_pieces() {
    // Bytes keep coming, but the whole answer passes the total deadline.
    let up = fake(|s, _| {
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n");
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(100));
            if s.write_all(b"x").is_err() {
                return;
            }
        }
    });
    let g = guard(&up, |c| c.deadline = Duration::from_millis(500));
    let t = Instant::now();
    let (status, body) = chat(&g);
    assert_eq!((status, content_of(&body).as_str()), (200, TRANSCRIPT));
    assert!(t.elapsed() < Duration::from_secs(2));
}

#[test]
fn fail_open_on_a_500_invalid_json_and_a_dropped_connection() {
    let up = fake(|s, req| {
        if req.contains("\"model\":\"s1-mini\"") {
            json_reply(s, "500 Internal Server Error", "{\"error\":\"boom\"}");
        }
    });
    assert_eq!(content_of(&chat(&guard(&up, |_| {})).1), TRANSCRIPT);

    let up = fake(|s, _| json_reply(s, "200 OK", "this is not json"));
    assert_eq!(content_of(&chat(&guard(&up, |_| {})).1), TRANSCRIPT);

    let up = fake(|s, _| json_reply(s, "200 OK", "{\"choices\":[]}"));
    assert_eq!(content_of(&chat(&guard(&up, |_| {})).1), TRANSCRIPT);

    // Accepts, reads the request, and closes without an answer.
    let up = fake(|_s, _| {});
    assert_eq!(content_of(&chat(&guard(&up, |_| {})).1), TRANSCRIPT);
}

#[test]
fn a_chat_request_without_a_transcript_gets_a_502_not_silence() {
    let g = guard(&closed_port(), |_| {});
    let (status, body) = raw(
        &g,
        &request("POST", "/v1/chat/completions", &g, "{\"messages\":[]}"),
    );
    assert_eq!(status, 502);
    assert!(body.contains("upstream unavailable"));
}

#[test]
fn passes_a_good_answer_and_reapplies_rules() {
    let up = fake(|s, _| json_reply(s, "200 OK", &completion("Add a task hive for this", "stop")));
    let (status, body) = chat(&guard(&up, |_| {}));
    assert_eq!(status, 200);
    assert_eq!(content_of(&body), "Add a Taskhive for this");
}

// G4: truncated answers

#[test]
fn finish_reason_length_falls_back_to_the_transcript() {
    // A cut-off answer that is not short and not too long, so only finish_reason shows it.
    let up = fake(|s, _| json_reply(s, "200 OK", &completion("add a Taskhive for", "length")));
    let (status, body) = chat(&guard(&up, |_| {}));
    assert_eq!(status, 200);
    assert_eq!(content_of(&body), TRANSCRIPT);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["choices"][0]["finish_reason"], "stop");
}

#[test]
fn a_much_shorter_answer_falls_back() {
    let long = "please write the long report about the new storage design and send it to the whole team before friday";
    let body =
        serde_json::json!({"model":"m","messages":[{"role":"user","content":long}]}).to_string();
    let up = fake(|s, _| {
        json_reply(
            s,
            "200 OK",
            &completion("please write the long report", "stop"),
        )
    });
    let g = guard(&up, |_| {});
    let (status, out) = raw(&g, &request("POST", "/v1/chat/completions", &g, &body));
    assert_eq!(status, 200);
    assert_eq!(content_of(&out), long);
}

// G2: limits

#[test]
fn a_huge_content_length_is_refused_without_reading_it() {
    let g = guard(&closed_port(), |_| {});
    let req = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: {g}\r\nContent-Length: 10000000000\r\n\r\n"
    );
    assert_eq!(raw(&g, req.as_bytes()).0, 413);
    let req = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: {g}\r\nContent-Length: 1048577\r\n\r\n"
    );
    assert_eq!(raw(&g, req.as_bytes()).0, 413);
    // The proxy still works.
    assert_eq!(content_of(&chat(&g).1), TRANSCRIPT);
}

#[test]
fn a_body_of_exactly_one_mib_is_accepted() {
    let up = fake(|s, _| json_reply(s, "200 OK", &completion("ok", "stop")));
    let g = guard(&up, |_| {});
    let pad = "a".repeat(1024 * 1024 - 100);
    let body = format!("{{\"pad\":\"{pad}\"}}");
    assert!(body.len() <= 1024 * 1024);
    let (status, _) = raw(&g, &request("POST", "/v1/chat/completions", &g, &body));
    assert_eq!(status, 200); // not refused for size (413); relayed to the fake upstream
}

#[test]
fn a_huge_header_is_refused() {
    let g = guard(&closed_port(), |_| {});
    let req = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: {g}\r\nX-Big: {}\r\n\r\n",
        "a".repeat(1024 * 1024)
    );
    assert_eq!(raw(&g, req.as_bytes()).0, 431);
    let req = format!("GET /{} HTTP/1.1\r\nHost: {g}\r\n\r\n", "a".repeat(100_000));
    assert_eq!(raw(&g, req.as_bytes()).0, 431);
}

#[test]
fn chunked_request_bodies_are_refused() {
    let g = guard(&closed_port(), |_| {});
    let req = format!("POST /v1/chat/completions HTTP/1.1\r\nHost: {g}\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n");
    assert_eq!(raw(&g, req.as_bytes()).0, 411);
}

#[test]
fn a_bad_content_length_is_refused() {
    let g = guard(&closed_port(), |_| {});
    let req =
        format!("POST /v1/chat/completions HTTP/1.1\r\nHost: {g}\r\nContent-Length: abc\r\n\r\n");
    assert_eq!(raw(&g, req.as_bytes()).0, 400);
}

#[test]
fn a_slow_client_gets_a_timeout() {
    let g = guard(&closed_port(), |c| {
        c.read_timeout = Duration::from_millis(300)
    });
    let mut s = TcpStream::connect(&g).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    // The head is never finished, and the client trickles one byte at a time.
    let t = Instant::now();
    let mut out = Vec::new();
    for b in b"POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\n" {
        if s.write_all(&[*b]).is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
        if t.elapsed() > Duration::from_secs(3) {
            break;
        }
    }
    let _ = s.read_to_end(&mut out);
    assert_eq!(parse(&out).0, 408);
    assert!(t.elapsed() < Duration::from_secs(3));
}

#[test]
fn too_many_open_connections_get_a_503() {
    let g = guard(&closed_port(), |c| c.read_timeout = Duration::from_secs(3));
    let idle: Vec<TcpStream> = (0..8).map(|_| TcpStream::connect(&g).unwrap()).collect();
    std::thread::sleep(Duration::from_millis(200));
    let (status, _) = raw(&g, b"GET /v1/models HTTP/1.1\r\n\r\n");
    assert_eq!(status, 503);
    drop(idle);
}

// G3: host and path allow-list

#[test]
fn only_two_routes_are_forwarded() {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let s2 = seen.clone();
    let up = fake(move |s, req| {
        s2.lock()
            .unwrap()
            .push(req.lines().next().unwrap_or("").to_string());
        json_reply(s, "200 OK", "{\"data\":[]}");
    });
    let g = guard(&up, |_| {});
    for (m, p) in [
        ("DELETE", "/api/delete"),
        ("POST", "/api/pull"),
        ("POST", "/api/create"),
        ("GET", "/api/tags"),
        ("GET", "/"),
        ("POST", "/v1/models"),
        ("GET", "/v1/chat/completions"),
        ("PUT", "/v1/chat/completions"),
    ] {
        assert_eq!(raw(&g, &request(m, p, &g, "{}")).0, 404, "{m} {p}");
    }
    assert!(
        seen.lock().unwrap().is_empty(),
        "nothing may reach the upstream: {:?}",
        seen.lock().unwrap()
    );
    let (status, body) = raw(&g, &request("GET", "/v1/models", &g, ""));
    assert_eq!(status, 200);
    assert!(body.contains("data"));
    assert_eq!(seen.lock().unwrap().as_slice(), ["GET /v1/models HTTP/1.1"]);
}

#[test]
fn the_host_header_must_be_local_and_is_sent_on_as_the_upstream_address() {
    let (tx, rx) = mpsc::channel::<String>();
    let tx = std::sync::Mutex::new(tx);
    let up = fake(move |s, req| {
        let _ = tx.lock().unwrap().send(req);
        json_reply(s, "200 OK", &completion("add a Taskhive for this", "stop"));
    });
    let g = guard(&up, |_| {});
    let port = g.rsplit(':').next().unwrap();
    for bad in [
        "evil.example",
        "evil.example:11436",
        "localhost.evil.example",
        "127.0.0.1.evil.example",
        "10.0.0.1",
        "[::1]",
        "localhost:abc",
    ] {
        assert_eq!(
            raw(
                &g,
                &request("POST", "/v1/chat/completions", bad, &chat_body())
            )
            .0,
            403,
            "{bad}"
        );
    }
    // No Host header at all.
    let body = chat_body();
    let req = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    assert_eq!(raw(&g, req.as_bytes()).0, 403);
    assert!(
        rx.try_recv().is_err(),
        "a refused request must not reach the upstream"
    );

    for good in [
        format!("localhost:{port}"),
        format!("127.0.0.1:{port}"),
        "LOCALHOST".to_string(),
    ] {
        let req = format!("POST /v1/chat/completions HTTP/1.1\r\nHost: {good}\r\nOrigin: http://evil.example\r\nReferer: http://evil.example/\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        assert_eq!(raw(&g, req.as_bytes()).0, 200, "{good}");
        let seen = rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .to_ascii_lowercase();
        assert!(seen.contains(&format!("host: {up}\r\n")), "{seen}");
        assert!(
            !seen.contains("origin:") && !seen.contains("referer:"),
            "{seen}"
        );
    }
}

// G6: log rotation

#[test]
fn the_log_rotates_at_one_mib_and_keeps_one_old_file() {
    let d = temp_dir("log");
    let log = d.join("requests.log");
    std::fs::write(&log, vec![b'x'; 1024 * 1024 + 10]).unwrap();
    let up = fake(|s, _| json_reply(s, "200 OK", &completion("add a Taskhive for this", "stop")));
    let l = log.clone();
    let g = guard(&up, move |c| c.log = Some(l));
    assert_eq!(chat(&g).0, 200);
    let old = d.join("requests.log.1");
    assert_eq!(std::fs::metadata(&old).unwrap().len(), 1024 * 1024 + 10);
    let now = std::fs::read_to_string(&log).unwrap();
    assert_eq!(now.lines().count(), 1, "{now}");
    assert!(now.contains("fallback=none"));
    // A second rotation replaces the old file; there is never a .2.
    std::fs::write(&log, vec![b'y'; 1024 * 1024]).unwrap();
    assert_eq!(chat(&g).0, 200);
    assert_eq!(std::fs::read(&old).unwrap()[0], b'y');
    assert!(!d.join("requests.log.2").exists());
    let _ = std::fs::remove_dir_all(&d);
}
