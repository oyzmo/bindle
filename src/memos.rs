//! Client for a Memos server (<https://usememos.com>).
//!
//! Everything here is GTK-free and blocking: `ui.rs` runs the request
//! functions on a worker thread via `gio::spawn_blocking`, the same way it
//! reads files, because a request to an unreachable server would otherwise
//! stall the main loop for as long as the connect timeout.
//!
//! Authentication is a single access token minted in the Memos web UI
//! (Settings → Access Tokens) and sent as `Authorization: Bearer …`; there
//! is no username/password anywhere in the app. The token lives in the
//! keyring (`crate::secrets`), never in the settings file.
//!
//! The URL-building and JSON-shaping helpers are pure and unit-tested; only
//! `send` touches the network. Memos has renamed these endpoints across
//! releases, so every path lives in one place here — `API`.

use std::fmt;
use std::time::Duration;

use serde::Deserialize;

/// Path prefix of the whole API. Bumping this is the first thing to try if
/// a future Memos release moves the endpoints again.
const API: &str = "/api/v1";

/// Memos requested per page. The server caps this at 1000.
pub const PAGE_SIZE: u32 = 50;

/// Give up on an unresponsive server rather than blocking a worker thread
/// (and, through the browse dialog, the user) indefinitely.
const TIMEOUT: Duration = Duration::from_secs(20);

/// Longest summary line shown in the browse list.
const SUMMARY_MAX: usize = 90;

/// Longest slug used in a cache file name, so paths stay manageable.
const SLUG_MAX: usize = 48;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Visibility {
    /// The default everywhere, including for values this build doesn't
    /// recognise: never widen a memo's audience by accident.
    #[default]
    Private,
    Protected,
    Public,
}

impl Visibility {
    pub const ALL: [Visibility; 3] =
        [Visibility::Private, Visibility::Protected, Visibility::Public];

    pub fn as_api(self) -> &'static str {
        match self {
            Visibility::Private => "PRIVATE",
            Visibility::Protected => "PROTECTED",
            Visibility::Public => "PUBLIC",
        }
    }

    /// Unknown values read as Private: a memo whose visibility this build
    /// doesn't understand should not be presented as world-readable.
    pub fn from_api(s: &str) -> Visibility {
        match s {
            "PUBLIC" => Visibility::Public,
            "PROTECTED" => Visibility::Protected,
            _ => Visibility::Private,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Visibility::Private => "Private",
            Visibility::Protected => "Protected — signed-in users",
            Visibility::Public => "Public — anyone",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Visibility::Private => "Private",
            Visibility::Protected => "Protected",
            Visibility::Public => "Public",
        }
    }
}

/// One memo, reduced to the fields Bindle edits or needs for identity.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Memo {
    /// Resource name, `memos/{uid}` — the identity every call is keyed on.
    pub name: String,
    pub content: String,
    pub visibility: Visibility,
    /// RFC 3339 timestamp, treated as opaque: only ever compared for
    /// equality, to notice that the memo changed on the server.
    pub update_time: String,
    pub create_time: String,
}

/// Where and how to reach the server. Built per request on the worker
/// thread so the token is not kept in the UI struct.
#[derive(Clone)]
pub struct Server {
    /// Normalized base URL, no trailing slash (see `normalize_base`).
    pub base: String,
    pub token: String,
    /// Accept any certificate. Opt-in, for a LAN server whose CA cannot be
    /// installed system-wide; see `agent` for what it gives up.
    pub insecure: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// No server URL configured yet.
    NoServer,
    /// No access token in the keyring.
    NoToken,
    /// The keyring itself refused — locked, or no Secret Service running.
    Keyring(String),
    /// 401/403 — the token is wrong, expired or revoked.
    Auth,
    /// 404 — the memo is gone (deleted or archived on the server).
    NotFound,
    /// Any other non-2xx response.
    Status(u16, String),
    /// Connection refused, DNS failure, timeout.
    Network(String),
    /// The server's TLS certificate did not verify. Split out from
    /// `Network` because the fix is entirely different and not guessable
    /// from rustls' wording.
    Certificate(String),
    /// A 2xx response whose body wasn't the JSON we expected.
    Parse(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoServer => write!(f, "No Memos server set — open Memos → Server Setup"),
            Error::NoToken => write!(f, "No access token saved — open Memos → Server Setup"),
            Error::Keyring(msg) => write!(f, "Could not use the keyring: {msg}"),
            Error::Auth => {
                write!(f, "Memos rejected the token — it may have expired; check Server Setup")
            }
            Error::NotFound => write!(f, "That memo no longer exists on the server"),
            Error::Status(code, body) if body.is_empty() => write!(f, "Memos returned HTTP {code}"),
            Error::Status(code, body) => write!(f, "Memos returned HTTP {code}: {body}"),
            Error::Network(msg) => write!(f, "Could not reach Memos: {msg}"),
            Error::Certificate(msg) => write!(
                f,
                "Could not verify the Memos server's certificate ({msg}). \
                 If it is signed by your own CA, install that CA system-wide \
                 (update-ca-trust on Fedora, update-ca-certificates on Debian) \
                 — Bindle trusts whatever the system trusts."
            ),
            Error::Parse(msg) => write!(f, "Unexpected reply from Memos: {msg}"),
        }
    }
}

// --- Pure helpers ---

/// Clean up a hand-typed server URL: add a scheme when none was given and
/// drop trailing slashes, so path concatenation below never doubles them.
/// A bare host gets https, because http would silently send the token in
/// the clear; LAN instances on plain http can still be reached by typing
/// the scheme explicitly.
pub fn normalize_base(url: &str) -> String {
    let url = url.trim();
    let with_scheme = if url.is_empty() || url.contains("://") {
        url.to_string()
    } else {
        format!("https://{url}")
    };
    with_scheme.trim_end_matches('/').to_string()
}

/// The `{uid}` of a `memos/{uid}` resource name.
pub fn memo_uid(name: &str) -> Option<&str> {
    name.strip_prefix("memos/").filter(|uid| !uid.is_empty())
}

/// Percent-encode a query parameter value. Deliberately conservative: only
/// unreserved characters survive, everything else is escaped.
pub fn encode_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// CEL filter matching memos containing `query`. Quotes and backslashes are
/// escaped so a search for `"` can't change the expression's shape.
pub fn search_filter(query: &str) -> String {
    let escaped = query.replace('\\', "\\\\").replace('"', "\\\"");
    format!("content.contains(\"{escaped}\")")
}

/// URL for one page of the memo list.
pub fn list_url(base: &str, page_token: &str, filter: Option<&str>) -> String {
    let mut url = format!("{base}{API}/memos?pageSize={PAGE_SIZE}");
    if !page_token.is_empty() {
        url.push_str(&format!("&pageToken={}", encode_query(page_token)));
    }
    if let Some(filter) = filter {
        url.push_str(&format!("&filter={}", encode_query(filter)));
    }
    url
}

/// URL of a single memo, from its `memos/{uid}` resource name.
pub fn memo_url(base: &str, name: &str) -> String {
    format!("{base}{API}/{name}")
}

/// Body of a create request. The HTTP body *is* the memo (the proto marks
/// `body: "memo"`), not a wrapper object around it.
pub fn create_body(content: &str, visibility: Visibility) -> String {
    serde_json::json!({ "content": content, "visibility": visibility.as_api() }).to_string()
}

/// Body of a content-only update. Paired with `?updateMask=content` so the
/// server leaves visibility, pin state and tags alone.
pub fn update_body(content: &str) -> String {
    serde_json::json!({ "content": content }).to_string()
}

/// One-line description of a memo for the browse list. Memos have no title
/// field, so this is the first line that carries text, stripped of the
/// markdown that would otherwise dominate short rows.
pub fn summary(content: &str) -> String {
    let line = content
        .lines()
        .map(strip_markers)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    truncate(&line, SUMMARY_MAX)
}

/// Strip leading block markers and inline emphasis from one line.
fn strip_markers(line: &str) -> String {
    let mut rest = line.trim();
    // Heading, quote and list markers, in any order and nesting.
    loop {
        let trimmed = rest
            .trim_start_matches(['#', '>', '-', '*', '+'])
            .trim_start();
        let trimmed = match trimmed.split_once(['.', ')']) {
            // An ordered-list marker: digits then the separator.
            Some((head, tail)) if !head.is_empty() && head.bytes().all(|b| b.is_ascii_digit()) => {
                tail.trim_start()
            }
            _ => trimmed,
        };
        if trimmed == rest {
            break;
        }
        rest = trimmed;
    }
    // Task-list checkbox left over from "- [ ] thing".
    for marker in ["[ ] ", "[x] ", "[X] "] {
        rest = rest.strip_prefix(marker).unwrap_or(rest);
    }
    rest.replace(['*', '_', '`', '~'], "").trim().to_string()
}

/// Truncate on a character boundary, marking that text was cut.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", head.trim_end())
}

/// File-name stem for a memo's local cache copy: a readable slug from its
/// first line plus the uid, which keeps names unique and lets the window
/// title say something better than a bare id.
pub fn cache_stem(memo: &Memo) -> String {
    // The uid comes from the server and ends up in a path. Real ones are
    // short alphanumeric ids; anything else is flattened, so a name like
    // `memos/../../x` can never steer the write out of the cache directory.
    let uid: String = memo_uid(&memo.name)
        .unwrap_or("memo")
        .chars()
        .take(64)
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let mut slug = String::new();
    let mut last_dash = true;
    for ch in truncate(&summary(&memo.content), SLUG_MAX).chars() {
        if ch.is_alphanumeric() {
            slug.push(ch);
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        format!("memo-{uid}")
    } else {
        format!("{slug}-{uid}")
    }
}

/// Date part of an RFC 3339 timestamp, for the browse list's subtitle.
pub fn short_date(timestamp: &str) -> String {
    timestamp.split('T').next().unwrap_or(timestamp).to_string()
}

// --- JSON ---

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct MemoJson {
    name: String,
    content: String,
    visibility: String,
    update_time: String,
    create_time: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct ListJson {
    memos: Vec<MemoJson>,
    next_page_token: String,
}

impl From<MemoJson> for Memo {
    fn from(json: MemoJson) -> Memo {
        Memo {
            name: json.name,
            content: json.content,
            visibility: Visibility::from_api(&json.visibility),
            update_time: json.update_time,
            create_time: json.create_time,
        }
    }
}

pub fn parse_memo(body: &str) -> Result<Memo, Error> {
    serde_json::from_str::<MemoJson>(body)
        .map(Memo::from)
        .map_err(|err| Error::Parse(err.to_string()))
}

/// Parse one page: the memos plus the token for the next page, if any.
pub fn parse_list(body: &str) -> Result<(Vec<Memo>, String), Error> {
    let list: ListJson =
        serde_json::from_str(body).map_err(|err| Error::Parse(err.to_string()))?;
    let memos = list.memos.into_iter().map(Memo::from).collect();
    Ok((memos, list.next_page_token))
}

/// Case-insensitive substring match over memo content, used when the server
/// rejects the CEL filter (see `list`).
pub fn matches(memo: &Memo, query: &str) -> bool {
    memo.content.to_lowercase().contains(&query.trim().to_lowercase())
}

// --- Transport ---

/// The HTTP agent for one request.
///
/// `insecure` turns off certificate verification entirely — not just the
/// issuer check, but the hostname check too, so the connection is encrypted
/// against passive listeners and against nothing else: anything that can
/// route the traffic can present its own certificate and read the bearer
/// token. It exists because a LAN server's CA cannot always be installed
/// system-wide; installing the CA is always the better fix, and the UI says
/// so at the switch and again whenever the setting is on.
fn agent(insecure: bool) -> ureq::Agent {
    ureq::Agent::config_builder()
        // Read the body of 4xx/5xx replies instead of losing them to an
        // early Err — Memos puts a useful message in there.
        .http_status_as_error(false)
        .timeout_global(Some(TIMEOUT))
        .user_agent(concat!("Bindle/", env!("CARGO_PKG_VERSION")))
        // Verify against the OS trust store, not ureq's default compiled-in
        // Mozilla bundle. Memos is self-hosted, so its certificate is often
        // signed by the user's own CA; with the default roots that fails as
        // UnknownIssuer even though the rest of the system trusts it.
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .disable_verification(insecure)
                .build(),
        )
        .build()
        .into()
}

/// Sort a failed request into the two cases the user can act on. rustls
/// reports certificate problems as plain I/O errors, so this matches on the
/// wording; a miss only costs the more generic message.
fn transport_error(err: ureq::Error) -> Error {
    let msg = err.to_string();
    if msg.contains("certificate") || msg.contains("CaUsedAsEndEntity") {
        Error::Certificate(msg)
    } else {
        Error::Network(msg)
    }
}

/// Perform one request and return its body, mapping the status to `Error`.
fn send(server: &Server, method: &str, url: &str, body: Option<String>) -> Result<String, Error> {
    if server.base.is_empty() {
        return Err(Error::NoServer);
    }
    if server.token.is_empty() {
        return Err(Error::NoToken);
    }
    let agent = agent(server.insecure);
    let bearer = format!("Bearer {}", server.token);
    // ureq types bodied and bodiless requests differently, so the two shapes
    // can't share one builder chain.
    let sent = match body {
        Some(body) => match method {
            "PATCH" => agent.patch(url),
            _ => agent.post(url),
        }
        .header("Authorization", &bearer)
        .header("Content-Type", "application/json")
        .send(&body),
        None => agent.get(url).header("Authorization", &bearer).call(),
    };
    let mut response = sent.map_err(transport_error)?;
    let status = response.status().as_u16();
    let text = response.body_mut().read_to_string().unwrap_or_default();
    match status {
        200..=299 => Ok(text),
        401 | 403 => Err(Error::Auth),
        404 => Err(Error::NotFound),
        _ => Err(Error::Status(status, truncate(text.trim(), 200))),
    }
}

/// One page of memos, newest first. `query` filters server-side; if the
/// server rejects the filter expression — the CEL dialect has changed
/// between Memos releases — the page is fetched unfiltered and matched
/// locally instead, so search degrades rather than breaking.
pub fn list(server: &Server, page_token: &str, query: &str) -> Result<(Vec<Memo>, String), Error> {
    let query = query.trim();
    if query.is_empty() {
        return parse_list(&send(server, "GET", &list_url(&server.base, page_token, None), None)?);
    }
    let filter = search_filter(query);
    match send(server, "GET", &list_url(&server.base, page_token, Some(&filter)), None) {
        Ok(body) => parse_list(&body),
        // Only a rejected *expression* falls back; auth and network errors
        // are real and must surface.
        Err(Error::Status(code, _)) if (400..500).contains(&code) => {
            let body = send(server, "GET", &list_url(&server.base, page_token, None), None)?;
            let (memos, next) = parse_list(&body)?;
            Ok((memos.into_iter().filter(|m| matches(m, query)).collect(), next))
        }
        Err(err) => Err(err),
    }
}

pub fn get(server: &Server, name: &str) -> Result<Memo, Error> {
    parse_memo(&send(server, "GET", &memo_url(&server.base, name), None)?)
}

pub fn create(server: &Server, content: &str, visibility: Visibility) -> Result<Memo, Error> {
    let url = format!("{}{API}/memos", server.base);
    parse_memo(&send(server, "POST", &url, Some(create_body(content, visibility)))?)
}

/// Replace a memo's content, leaving its other fields untouched.
pub fn update(server: &Server, name: &str, content: &str) -> Result<Memo, Error> {
    let url = format!("{}?updateMask=content", memo_url(&server.base, name));
    parse_memo(&send(server, "PATCH", &url, Some(update_body(content)))?)
}

/// Outcome of a guarded push.
#[derive(Debug)]
pub enum Push {
    Done(Memo),
    /// The memo changed on the server since it was fetched; only the caller
    /// can decide whether replacing that version is right.
    Conflict(Memo),
}

/// Replace a memo's content, refusing if it moved underneath us. With
/// `expected` set the memo's current timestamp is compared first: a blind
/// PATCH would silently discard an edit made in the web UI or on another
/// device meanwhile. `None` skips the check — that is the second pass,
/// after the user has chosen to overwrite.
pub fn push(
    server: &Server,
    name: &str,
    content: &str,
    expected: Option<&str>,
) -> Result<Push, Error> {
    if let Some(expected) = expected {
        let current = get(server, name)?;
        if current.update_time != expected {
            return Ok(Push::Conflict(current));
        }
    }
    update(server, name, content).map(Push::Done)
}

/// Cheapest call that proves the URL and token are both good.
pub fn check(server: &Server) -> Result<usize, Error> {
    let url = format!("{}{API}/memos?pageSize=1", server.base);
    let (memos, _) = parse_list(&send(server, "GET", &url, None)?)?;
    Ok(memos.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_urls() {
        assert_eq!(normalize_base("memos.example.com"), "https://memos.example.com");
        assert_eq!(normalize_base("  https://m.example.com/  "), "https://m.example.com");
        assert_eq!(normalize_base("http://192.168.1.9:5230"), "http://192.168.1.9:5230");
        assert_eq!(normalize_base("https://x.org///"), "https://x.org");
        assert_eq!(normalize_base(""), "");
    }

    #[test]
    fn extracts_uid() {
        assert_eq!(memo_uid("memos/abc123"), Some("abc123"));
        assert_eq!(memo_uid("memos/"), None);
        assert_eq!(memo_uid("notes/abc"), None);
    }

    #[test]
    fn encodes_query_values() {
        assert_eq!(encode_query("plain"), "plain");
        assert_eq!(encode_query("a b"), "a%20b");
        assert_eq!(encode_query("content.contains(\"x\")"), "content.contains%28%22x%22%29");
        // Multi-byte characters are encoded per UTF-8 byte.
        assert_eq!(encode_query("ø"), "%C3%B8");
    }

    #[test]
    fn escapes_filter_quotes() {
        assert_eq!(search_filter("todo"), "content.contains(\"todo\")");
        assert_eq!(search_filter("say \"hi\""), "content.contains(\"say \\\"hi\\\"\")");
        assert_eq!(search_filter("back\\slash"), "content.contains(\"back\\\\slash\")");
    }

    #[test]
    fn builds_list_urls() {
        let base = "https://m.example.com";
        assert_eq!(list_url(base, "", None), "https://m.example.com/api/v1/memos?pageSize=50");
        assert_eq!(
            list_url(base, "tok en", None),
            "https://m.example.com/api/v1/memos?pageSize=50&pageToken=tok%20en"
        );
        assert!(list_url(base, "", Some("content.contains(\"a\")")).contains("&filter=content."));
    }

    #[test]
    fn builds_memo_urls() {
        assert_eq!(memo_url("https://m.example.com", "memos/x1"), "https://m.example.com/api/v1/memos/x1");
    }

    #[test]
    fn builds_bodies() {
        let body = create_body("hello \"world\"", Visibility::Public);
        assert!(body.contains(r#""visibility":"PUBLIC""#));
        // Serialization escapes the quotes rather than producing broken JSON.
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["content"],
            "hello \"world\""
        );
        assert_eq!(update_body("x"), r#"{"content":"x"}"#);
    }

    #[test]
    fn summarizes_first_meaningful_line() {
        assert_eq!(summary("# Title\n\nbody"), "Title");
        assert_eq!(summary("\n\n  - [ ] buy **milk**"), "buy milk");
        assert_eq!(summary("> quoted note"), "quoted note");
        assert_eq!(summary("1. first item"), "first item");
        assert_eq!(summary(""), "");
        assert_eq!(summary("   \n  \t "), "");
        let long = "w".repeat(200);
        let summarized = summary(&long);
        assert_eq!(summarized.chars().count(), SUMMARY_MAX);
        assert!(summarized.ends_with('…'));
    }

    #[test]
    fn summary_keeps_multibyte_text_intact() {
        assert_eq!(summary("# Blåbærsyltetøy"), "Blåbærsyltetøy");
        // Truncation must land on a character boundary, not split a char.
        let text = "æ".repeat(200);
        assert!(summary(&text).chars().all(|c| c == 'æ' || c == '…'));
    }

    #[test]
    fn builds_cache_stems() {
        let memo = Memo {
            name: "memos/x1".into(),
            content: "# Meeting notes: Q3!\nbody".into(),
            ..Memo::default()
        };
        assert_eq!(cache_stem(&memo), "Meeting-notes-Q3-x1");
        // An empty or symbol-only memo still yields a usable file name.
        let empty = Memo { name: "memos/x2".into(), content: "***".into(), ..Memo::default() };
        assert_eq!(cache_stem(&empty), "memo-x2");
        let unnamed = Memo { content: "hi".into(), ..Memo::default() };
        assert_eq!(cache_stem(&unnamed), "hi-memo");
    }

    #[test]
    fn a_hostile_uid_cannot_leave_the_cache_directory() {
        let memo = Memo {
            name: "memos/../../../.config/autostart/x".into(),
            content: "hi".into(),
            ..Memo::default()
        };
        let stem = cache_stem(&memo);
        assert!(!stem.contains('/') && !stem.contains('.'), "{stem}");
        assert!(stem.starts_with("hi-") && stem.ends_with("config_autostart_x"), "{stem}");
    }

    #[test]
    fn parses_a_list_page() {
        let body = r#"{
            "memos": [
              {"name":"memos/a1","content":"first","visibility":"PUBLIC",
               "updateTime":"2026-08-14T10:00:00Z","createTime":"2026-08-01T09:00:00Z"},
              {"name":"memos/a2","content":"second","visibility":"PRIVATE",
               "updateTime":"2026-08-13T10:00:00Z","createTime":"2026-08-02T09:00:00Z"}
            ],
            "nextPageToken": "page2"
        }"#;
        let (memos, next) = parse_list(body).unwrap();
        assert_eq!(next, "page2");
        assert_eq!(memos.len(), 2);
        assert_eq!(memos[0].name, "memos/a1");
        assert_eq!(memos[0].visibility, Visibility::Public);
        assert_eq!(memos[1].visibility, Visibility::Private);
        assert_eq!(memos[0].update_time, "2026-08-14T10:00:00Z");
    }

    #[test]
    fn parses_an_empty_list() {
        let (memos, next) = parse_list("{}").unwrap();
        assert!(memos.is_empty());
        assert!(next.is_empty());
    }

    #[test]
    fn unknown_visibility_reads_as_private() {
        // A future server value must never widen a memo's audience.
        let memo = parse_memo(r#"{"name":"memos/a","visibility":"WORKSPACE"}"#).unwrap();
        assert_eq!(memo.visibility, Visibility::Private);
    }

    #[test]
    fn rejects_non_json_bodies() {
        // An HTML error page from a reverse proxy must not look like success.
        assert!(matches!(parse_list("<html>502</html>"), Err(Error::Parse(_))));
    }

    #[test]
    fn matches_case_insensitively() {
        let memo = Memo { content: "Buy Milk".into(), ..Memo::default() };
        assert!(matches(&memo, "milk"));
        assert!(matches(&memo, "  BUY "));
        assert!(!matches(&memo, "bread"));
    }

    #[test]
    fn errors_explain_themselves() {
        assert!(Error::Auth.to_string().contains("expired"));
        assert!(Error::Status(502, String::new()).to_string().contains("502"));
        // A certificate failure must say what to do about it: rustls' own
        // wording ("invalid peer certificate: UnknownIssuer") does not.
        assert!(Error::Certificate("UnknownIssuer".into()).to_string().contains("own CA"));
    }

    #[test]
    fn the_agent_trusts_the_system_store() {
        // Self-hosted Memos instances commonly sit behind a private CA.
        // ureq's default roots are a compiled-in Mozilla bundle that cannot
        // see one, however correctly it is installed — so this must stay on
        // the platform verifier. Dropping the `platform-verifier` feature
        // would turn this into a runtime panic rather than a silent
        // downgrade, but the setting itself is easy to "tidy away".
        let roots = format!("{:?}", agent(false).config().tls_config().root_certs());
        assert_eq!(roots, "PlatformVerifier");
    }

    #[test]
    fn verification_is_on_unless_explicitly_disabled() {
        // The insecure path must be reachable only through the flag: a
        // default that silently skipped verification would be invisible.
        assert!(!agent(false).config().tls_config().disable_verification());
        assert!(agent(true).config().tls_config().disable_verification());
    }

    #[test]
    fn certificate_failures_are_not_reported_as_plain_network_errors() {
        // The two have completely different fixes, so they must not share a
        // message; rustls surfaces cert problems as I/O errors.
        let cert = Error::Certificate("invalid peer certificate: UnknownIssuer".into());
        assert!(cert.to_string().contains("update-ca-trust"));
        assert!(!Error::Network("connection refused".into()).to_string().contains("CA"));
    }

    #[test]
    fn shortens_dates() {
        assert_eq!(short_date("2026-08-14T10:00:00Z"), "2026-08-14");
        assert_eq!(short_date(""), "");
    }
}
