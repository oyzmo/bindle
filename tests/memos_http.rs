//! The Memos transport against a stub server on a real socket.
//!
//! The unit tests in `memos.rs` cover URL and JSON shaping; this checks the
//! part they can't — that the requests actually put the token, method, path
//! and body on the wire the way the API expects, and that response statuses
//! map to the right `Error`. No display needed, so it runs anywhere.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use bindle::memos::{self, Error, Server, Visibility};

/// One request as the stub saw it.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    auth: String,
    body: String,
}

/// A throwaway HTTP server that replies with `responses` in order and
/// records what it was asked. Every reply closes the connection, so each
/// request lands on its own `accept` instead of being pipelined onto one.
struct Stub {
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

fn stub(responses: Vec<(u16, &'static str)>) -> Stub {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = seen.clone();
    // Signals that the listener thread is running before the test connects.
    let (ready, started) = mpsc::channel();
    thread::spawn(move || {
        ready.send(()).ok();
        for (status, body) in responses {
            let Ok((stream, _)) = listener.accept() else { return };
            serve(stream, status, body, &recorder);
        }
    });
    started.recv().expect("listener start");
    Stub { base: format!("http://127.0.0.1:{port}"), seen }
}

fn serve(mut stream: TcpStream, status: u16, body: &str, seen: &Arc<Mutex<Vec<Seen>>>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut start = String::new();
    reader.read_line(&mut start).expect("request line");
    let mut parts = start.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let mut auth = String::new();
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        let lower = line.to_lowercase();
        if let Some(value) = lower.strip_prefix("authorization:") {
            auth = value.trim().to_string();
        }
        if let Some(value) = lower.strip_prefix("content-length:") {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut payload = vec![0u8; length];
    reader.read_exact(&mut payload).expect("body");

    seen.lock().unwrap().push(Seen {
        method,
        path,
        auth,
        body: String::from_utf8_lossy(&payload).to_string(),
    });

    let response = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).ok();
    stream.flush().ok();
}

impl Stub {
    fn server(&self) -> Server {
        Server { base: self.base.clone(), token: "tok-123".into(), insecure: false }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

const ONE_MEMO: &str = r#"{"name":"memos/a1","content":"hello","visibility":"PRIVATE",
    "updateTime":"2026-08-14T10:00:00Z","createTime":"2026-08-14T09:00:00Z"}"#;

#[test]
fn create_posts_the_memo_as_the_body() {
    let stub = stub(vec![(200, ONE_MEMO)]);
    let memo = memos::create(&stub.server(), "hello", Visibility::Public).expect("create");
    assert_eq!(memo.name, "memos/a1");

    let seen = stub.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].path, "/api/v1/memos");
    // The token goes out as a bearer credential, never as basic auth.
    assert_eq!(seen[0].auth, "bearer tok-123");
    // `body: "memo"` in the proto: the memo itself, not a wrapper.
    let body: serde_json::Value = serde_json::from_str(&seen[0].body).expect("json body");
    assert_eq!(body["content"], "hello");
    assert_eq!(body["visibility"], "PUBLIC");
    assert!(body.get("memo").is_none());
}

#[test]
fn list_requests_a_page_and_reports_the_next_token() {
    let page = r#"{"memos":[{"name":"memos/a1","content":"one"}],"nextPageToken":"tok2"}"#;
    let stub = stub(vec![(200, page)]);
    let (memos, next) = memos::list(&stub.server(), "", "").expect("list");
    assert_eq!(memos.len(), 1);
    assert_eq!(next, "tok2");

    let seen = stub.seen();
    assert_eq!(seen[0].method, "GET");
    assert!(seen[0].path.starts_with("/api/v1/memos?pageSize="));
    assert!(!seen[0].path.contains("filter="), "no filter without a query");
    assert!(seen[0].body.is_empty());
}

#[test]
fn search_sends_a_filter() {
    let page = r#"{"memos":[{"name":"memos/a1","content":"buy milk"}]}"#;
    let stub = stub(vec![(200, page)]);
    let (memos, _) = memos::list(&stub.server(), "", "milk").expect("list");
    assert_eq!(memos.len(), 1);
    assert!(stub.seen()[0].path.contains("filter=content.contains"));
}

#[test]
fn search_falls_back_to_local_matching_when_the_filter_is_rejected() {
    // Memos has changed its CEL dialect across releases; a server that
    // refuses the expression must degrade to a local match, not an error.
    let page = r#"{"memos":[
        {"name":"memos/a1","content":"buy milk"},
        {"name":"memos/a2","content":"call the plumber"}]}"#;
    let stub = stub(vec![(400, r#"{"message":"bad filter"}"#), (200, page)]);
    let (memos, _) = memos::list(&stub.server(), "", "MILK").expect("list");

    let seen = stub.seen();
    assert_eq!(seen.len(), 2, "one filtered attempt, then one plain");
    assert!(seen[0].path.contains("filter="));
    assert!(!seen[1].path.contains("filter="));
    // Only the matching memo survives, case-insensitively.
    assert_eq!(memos.len(), 1);
    assert_eq!(memos[0].name, "memos/a1");
}

#[test]
fn a_rejected_token_is_not_retried_as_a_filter_problem() {
    // 401 during a search must surface, not trigger the unfiltered retry.
    let stub = stub(vec![(401, r#"{"message":"unauthorized"}"#)]);
    assert_eq!(memos::list(&stub.server(), "", "milk"), Err(Error::Auth));
    assert_eq!(stub.seen().len(), 1);
}

#[test]
fn update_patches_content_behind_an_update_mask() {
    let stub = stub(vec![(200, ONE_MEMO)]);
    memos::update(&stub.server(), "memos/a1", "new text").expect("update");

    let seen = stub.seen();
    assert_eq!(seen[0].method, "PATCH");
    assert_eq!(seen[0].path, "/api/v1/memos/a1?updateMask=content");
    // Without the mask the server would blank visibility, pins and tags.
    let body: serde_json::Value = serde_json::from_str(&seen[0].body).expect("json body");
    assert_eq!(body["content"], "new text");
    assert_eq!(body.as_object().unwrap().len(), 1);
}

#[test]
fn push_checks_the_timestamp_before_replacing() {
    // The memo is unchanged since it was fetched, so the PATCH goes ahead.
    let stub = stub(vec![(200, ONE_MEMO), (200, ONE_MEMO)]);
    let outcome = memos::push(&stub.server(), "memos/a1", "edited", Some("2026-08-14T10:00:00Z"))
        .expect("push");
    assert!(matches!(outcome, memos::Push::Done(_)));

    let seen = stub.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].method, "GET");
    assert_eq!(seen[1].method, "PATCH");
}

#[test]
fn push_refuses_when_the_memo_moved_underneath_it() {
    // This is the case that would otherwise silently eat someone's edit:
    // the server's copy is newer than the one that was checked out.
    let stub = stub(vec![(200, ONE_MEMO)]);
    let outcome = memos::push(&stub.server(), "memos/a1", "edited", Some("2026-08-01T00:00:00Z"))
        .expect("push");
    match outcome {
        memos::Push::Conflict(current) => assert_eq!(current.content, "hello"),
        other => panic!("expected a conflict, got {other:?}"),
    }
    // Crucially, nothing was written.
    let seen = stub.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "GET");
}

#[test]
fn push_without_an_expected_timestamp_overwrites_directly() {
    // The second pass, after the user chose to overwrite: no check, no GET.
    let stub = stub(vec![(200, ONE_MEMO)]);
    let outcome = memos::push(&stub.server(), "memos/a1", "edited", None).expect("push");
    assert!(matches!(outcome, memos::Push::Done(_)));

    let seen = stub.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "PATCH");
}

#[test]
fn a_failed_check_never_becomes_a_write() {
    // If the pre-push GET fails, the PATCH must not be attempted anyway.
    let stub = stub(vec![(500, "upstream down")]);
    assert!(memos::push(&stub.server(), "memos/a1", "edited", Some("x")).is_err());
    assert_eq!(stub.seen().len(), 1);
}

#[test]
fn get_fetches_one_memo_by_resource_name() {
    let stub = stub(vec![(200, ONE_MEMO)]);
    let memo = memos::get(&stub.server(), "memos/a1").expect("get");
    assert_eq!(memo.update_time, "2026-08-14T10:00:00Z");
    assert_eq!(stub.seen()[0].path, "/api/v1/memos/a1");
}

#[test]
fn statuses_map_to_distinguishable_errors() {
    assert_eq!(memos::get(&stub(vec![(403, "{}")]).server(), "memos/a"), Err(Error::Auth));
    assert_eq!(memos::get(&stub(vec![(404, "{}")]).server(), "memos/a"), Err(Error::NotFound));
    let stub = stub(vec![(500, "boom")]);
    match memos::get(&stub.server(), "memos/a") {
        Err(Error::Status(500, body)) => assert_eq!(body, "boom"),
        other => panic!("expected a 500 status error, got {other:?}"),
    }
}

#[test]
fn an_html_error_page_is_a_parse_error_not_a_success() {
    // A reverse proxy in front of Memos can answer 200 with HTML; that must
    // not read as an empty memo list.
    let stub = stub(vec![(200, "<html><body>hi</body></html>")]);
    assert!(matches!(memos::list(&stub.server(), "", ""), Err(Error::Parse(_))));
}

#[test]
fn a_missing_server_or_token_never_reaches_the_network() {
    let no_server = Server { base: String::new(), token: "t".into(), insecure: false };
    assert_eq!(memos::check(&no_server), Err(Error::NoServer));
    let no_token = Server { base: "http://127.0.0.1:1".into(), token: String::new(), insecure: false };
    assert_eq!(memos::check(&no_token), Err(Error::NoToken));
}

#[test]
fn an_unreachable_server_is_a_network_error() {
    // Port 1 on loopback refuses immediately, so this stays fast.
    let server = Server { base: "http://127.0.0.1:1".into(), token: "t".into(), insecure: false };
    assert!(matches!(memos::check(&server), Err(Error::Network(_))));
}
