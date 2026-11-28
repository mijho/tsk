//! The HTTP side of `tsk-sync`: `GET /health` and `POST /ops`.

use std::io::Read;
use std::net::SocketAddr;

use serde::Deserialize;
use tiny_http::{Header, Method, Request, Response, Server};

use super::authority::{Authority, OpResult};
use super::tokens::Scope;
use crate::domain::Op;

/// The largest `POST /ops` body the server reads.
const MAX_BODY: u64 = 4 << 20;

/// The most ops one `GET /ops` page returns, and the default page size.
pub const MAX_PAGE: usize = 500;

/// A `POST /ops` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpsRequest {
    ops: Vec<Op>,
}

type Reply = Response<std::io::Cursor<Vec<u8>>>;

/// A bound server, ready to answer requests.
pub struct SyncServer {
    http: Server,
    authority: Authority,
}

impl SyncServer {
    /// Bind `listen` (port 0 picks a free port) over a loaded authority.
    pub fn bind(listen: SocketAddr, authority: Authority) -> std::io::Result<Self> {
        let http = Server::http(listen).map_err(|error| {
            std::io::Error::other(format!("cannot listen on {listen}: {error}"))
        })?;
        Ok(Self { http, authority })
    }

    /// The address actually bound.
    pub fn local_addr(&self) -> SocketAddr {
        self.http
            .server_addr()
            .to_ip()
            .expect("tsk-sync binds an IP address")
    }

    /// Answer requests until the listener fails, which is an error: a server never just stops.
    pub fn serve(&mut self) -> std::io::Result<()> {
        loop {
            self.serve_one()?;
        }
    }

    /// Answer one request. A client that hung up before its answer is logged, not fatal.
    pub fn serve_one(&mut self) -> std::io::Result<()> {
        let mut request = self.http.recv()?;
        let reply = self.respond(&mut request);
        let (method, url) = (request.method().clone(), request.url().to_string());
        if let Err(error) = request.respond(reply) {
            eprintln!("tsk-sync: could not answer {method} {url}: {error}");
        }
        Ok(())
    }

    fn respond(&mut self, request: &mut Request) -> Reply {
        match (request.method(), request.url()) {
            (Method::Get, "/health") => match self.authority.op_count() {
                Ok(ops) => json(
                    200,
                    serde_json::json!({
                        "protocol_version": super::PROTOCOL_VERSION,
                        "min_protocol_version": super::MIN_PROTOCOL_VERSION,
                        "seq": self.authority.seq(),
                        "ops": ops,
                    }),
                ),
                Err(error) => failed(&error),
            },
            _ => self.authorized(request),
        }
    }

    /// Every endpoint but `/health` needs a bearer token; pushing needs write scope.
    fn authorized(&mut self, request: &mut Request) -> Reply {
        if let Err(refusal) = protocol(request) {
            return refusal;
        }
        let token = match bearer(request) {
            Ok(token) => token,
            Err(refusal) => return refusal,
        };
        let grant = match self.authority.grant(&token) {
            Ok(Some(grant)) => grant,
            Ok(None) => return text(401, "that token is not one this server issued"),
            Err(error) => return failed(&error),
        };
        match (request.method(), request.url()) {
            (Method::Post, "/ops") if grant.scope != Scope::Write => text(
                403,
                &format!("{}'s token is read-only: it cannot push ops", grant.actor),
            ),
            (Method::Post, "/ops") => self.post_ops(request, &grant.actor),
            (Method::Get, "/snapshot") => json(
                200,
                serde_json::json!({
                    "seq": self.authority.seq(),
                    "state": self.authority.state(),
                }),
            ),
            (Method::Get, url) if url == "/ops" || url.starts_with("/ops?") => self.get_ops(url),
            _ => text(404, "not found"),
        }
    }

    /// `GET /ops?after=N&limit=M`: accepted ops after `seq` N in order; `next` is the seq to
    /// ask after for the following page, absent on the last one.
    fn get_ops(&self, url: &str) -> Reply {
        let (after, limit) = match page(url) {
            Ok(page) => page,
            Err(reason) => return text(400, &reason),
        };
        if after > self.authority.seq() {
            // A client past the server's log means the server's store was reset or lost.
            return text(
                409,
                &format!(
                    "after {after} is past this server's last op {}: its store is not the one \
                     you synced with",
                    self.authority.seq()
                ),
            );
        }
        // One row past the page says whether another page follows.
        match self.authority.ops_after(after, limit + 1) {
            Ok(mut ops) => {
                let next = (ops.len() > limit).then(|| {
                    ops.truncate(limit);
                    ops.last().map(|(seq, _)| *seq)
                });
                let ops: Vec<_> = ops
                    .into_iter()
                    .map(|(seq, op)| serde_json::json!({ "seq": seq, "op": op }))
                    .collect();
                json(
                    200,
                    serde_json::json!({ "ops": ops, "next": next.flatten() }),
                )
            }
            Err(error) => failed(&error),
        }
    }

    fn post_ops(&mut self, request: &mut Request, actor: &str) -> Reply {
        let mut body = Vec::new();
        let read = request
            .as_reader()
            .take(MAX_BODY + 1)
            .read_to_end(&mut body);
        if let Err(error) = read {
            return text(400, &format!("cannot read the body: {error}"));
        }
        if body.len() as u64 > MAX_BODY {
            return text(413, &format!("the body is over {MAX_BODY} bytes"));
        }
        let parsed: OpsRequest = match serde_json::from_slice(&body) {
            Ok(parsed) => parsed,
            Err(error) => return text(400, &format!("not an ops batch: {error}")),
        };
        match self.authority.submit(actor, &parsed.ops) {
            Ok(results) => json(200, serde_json::json!({ "results": results })),
            Err(error) => failed(&error),
        }
    }
}

fn json(status: u16, body: serde_json::Value) -> Reply {
    Response::from_string(body.to_string())
        .with_status_code(status)
        .with_header(Header::from_bytes("Content-Type", "application/json").expect("header"))
}

/// The `after` and `limit` of a `GET /ops` query. `after` is required; `limit` defaults to
/// and is capped at [`MAX_PAGE`]. Unknown or malformed parameters refuse.
fn page(url: &str) -> Result<(i64, usize), String> {
    let query = url.split_once('?').map_or("", |(_, query)| query);
    let (mut after, mut limit) = (None, None);
    // Plain decimal digits only: no sign, no spaces, no encoding.
    let digits = |value: &str| {
        (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| value.parse::<u64>().ok())
            .flatten()
    };
    for pair in query.split('&') {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| format!("query parameter `{pair}` needs a value"))?;
        let slot = match key {
            "after" => &mut after,
            "limit" => &mut limit,
            other => return Err(format!("unknown query parameter `{other}`")),
        };
        if slot.is_some() {
            return Err(format!("`{key}` is given twice"));
        }
        *slot =
            Some(digits(value).ok_or_else(|| format!("`{key}` must be a whole number: {value}"))?);
    }
    let after = after.ok_or("`after` is required: /ops?after=N")?;
    let after = i64::try_from(after).map_err(|_| format!("`after` is too large: {after}"))?;
    let limit = match limit {
        None => MAX_PAGE,
        Some(limit) if (1..=MAX_PAGE as u64).contains(&limit) => limit as usize,
        Some(limit) => return Err(format!("`limit` must be 1 to {MAX_PAGE}: {limit}")),
    };
    Ok((after, limit))
}

/// The request's protocol version must be one this server speaks: missing, malformed, below
/// the minimum, or above this server's version answers 426 naming both sides.
fn protocol(request: &Request) -> Result<(), Reply> {
    let given: Vec<&str> = request
        .headers()
        .iter()
        .filter(|header| header.field.equiv(super::PROTOCOL_HEADER))
        .map(|header| header.value.as_str().trim())
        .collect();
    let client = match given.as_slice() {
        [value] => value.parse::<u32>().ok(),
        _ => None,
    };
    let speaks = client.is_some_and(|client| {
        (super::MIN_PROTOCOL_VERSION..=super::PROTOCOL_VERSION).contains(&client)
    });
    if speaks {
        return Ok(());
    }
    Err(json(
        426,
        serde_json::json!({
            "error": format!(
                "send {}: <n> with n from {} to {}",
                super::PROTOCOL_HEADER,
                super::MIN_PROTOCOL_VERSION,
                super::PROTOCOL_VERSION
            ),
            "client": given.first(),
            "minimum": super::MIN_PROTOCOL_VERSION,
            "server": super::PROTOCOL_VERSION,
        }),
    ))
}

/// The one bearer token the request carries. The scheme is case-insensitive; a missing,
/// repeated, or malformed `Authorization` header refuses.
fn bearer(request: &Request) -> Result<String, Reply> {
    let given: Vec<&str> = request
        .headers()
        .iter()
        .filter(|header| header.field.equiv("Authorization"))
        .map(|header| header.value.as_str())
        .collect();
    let value = match given.as_slice() {
        [] => {
            return Err(text(
                401,
                "a bearer token is required: Authorization: Bearer <token>",
            ))
        }
        [value] => value.trim(),
        _ => return Err(text(400, "send one Authorization header, not several")),
    };
    let token = value
        .split_once(' ')
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token.trim())
        .filter(|token| !token.is_empty() && !token.contains(char::is_whitespace));
    token.map(str::to_string).ok_or_else(|| {
        text(
            401,
            "a bearer token is required: Authorization: Bearer <token>",
        )
    })
}

/// A server-side failure: said to the client and logged here, never only one of the two.
fn failed(error: &super::db::DbError) -> Reply {
    eprintln!("tsk-sync: {error}");
    text(500, &error.to_string())
}

fn text(status: u16, body: &str) -> Reply {
    Response::from_string(body).with_status_code(status)
}

/// What a batch returns, for clients and tests.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OpsResponse {
    pub results: Vec<OpResult>,
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::domain::{Intent, ProvenanceOrigin, TaskScope};

    fn request(
        addr: SocketAddr,
        token: Option<&str>,
        method: &str,
        path: &str,
        body: &str,
    ) -> (u16, String) {
        let auth = token.map_or_else(String::new, |token| {
            format!("Authorization: Bearer {token}\r\n")
        });
        raw(
            addr,
            &format!("X-Tsk-Protocol: 1\r\n{auth}"),
            method,
            path,
            body,
        )
    }

    /// One request with exactly `headers` (each ending in `\r\n`) besides the framing.
    fn raw(addr: SocketAddr, headers: &str, method: &str, path: &str, body: &str) -> (u16, String) {
        let mut stream = std::net::TcpStream::connect(addr).expect("connect");
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: tsk\r\nConnection: close\r\n{headers}\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .expect("request");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("response");
        let status = response[9..12].parse().expect("status");
        let body = response
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.to_string())
            .unwrap_or_default();
        (status, body)
    }

    #[test]
    fn post_ops_answers_per_op_and_health_reports_the_seq() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        super::super::db::migrate(&mut db).unwrap();
        let token = super::super::tokens::add(&db, "you", Scope::Write).unwrap();
        let authority = Authority::load(db).unwrap();
        let mut server = SyncServer::bind("127.0.0.1:0".parse().unwrap(), authority).unwrap();
        let addr = server.local_addr();
        let serving = std::thread::spawn(move || {
            for _ in 0..5 {
                server.serve_one().expect("serve");
            }
        });
        let task = uuid::Uuid::new_v4();
        let op = Op::local(
            "you",
            Intent::Create {
                task,
                title: "over the wire".into(),
                notes: None,
                scope: TaskScope::Global,
                provenance: ProvenanceOrigin::Manual,
                thread: None,
                assignee: None,
                base: None,
                due: None,
                after: Vec::new(),
                steps: Vec::new(),
            },
        );
        let batch = serde_json::json!({ "ops": [op] }).to_string();
        let (status, body) = request(addr, Some(&token), "POST", "/ops", &batch);
        assert_eq!(status, 200, "{body}");
        let response: OpsResponse = serde_json::from_str(&body).expect("results");
        assert_eq!(
            response.results,
            [OpResult::Accepted {
                op_id: op.id,
                seq: 1,
                numbers: [(task, 1)].into(),
            }]
        );
        let (status, body) = request(addr, Some(&token), "GET", "/health", "");
        assert_eq!(status, 200);
        assert_eq!(
            body,
            r#"{"min_protocol_version":1,"ops":1,"protocol_version":1,"seq":1}"#
        );
        let (status, body) = request(
            addr,
            Some(&token),
            "POST",
            "/ops",
            r#"{"ops": [], "extra": 1}"#,
        );
        assert_eq!(status, 400, "{body}");
        assert!(body.starts_with("not an ops batch"), "{body}");
        assert_eq!(
            request(addr, Some(&token), "POST", "/ops", "not json").0,
            400
        );
        assert_eq!(request(addr, Some(&token), "GET", "/nope", "").0, 404);
        serving.join().expect("server thread");
    }

    #[test]
    fn snapshot_and_ops_after_page_through_the_log_and_refuse_bad_queries() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        super::super::db::migrate(&mut db).unwrap();
        let token = super::super::tokens::add(&db, "you", Scope::Write).unwrap();
        let mut authority = Authority::load(db).unwrap();
        let create = |title: &str| {
            Op::local(
                "you",
                Intent::Create {
                    task: uuid::Uuid::new_v4(),
                    title: title.into(),
                    notes: None,
                    scope: TaskScope::Global,
                    provenance: ProvenanceOrigin::Manual,
                    thread: None,
                    assignee: None,
                    base: None,
                    due: None,
                    after: Vec::new(),
                    steps: Vec::new(),
                },
            )
        };
        let ops = [create("one"), create("two"), create("three")];
        authority.submit("you", &ops).unwrap();
        let state = serde_json::to_value(authority.state()).unwrap();
        let mut server = SyncServer::bind("127.0.0.1:0".parse().unwrap(), authority).unwrap();
        let addr = server.local_addr();
        let bad = [
            "/ops",
            "/ops?after=-1",
            "/ops?after=x",
            "/ops?after=0&limit=0",
            "/ops?after=0&limit=501",
            "/ops?after=0&since=1",
            "/ops?after",
        ];
        let serving = std::thread::spawn(move || {
            for _ in 0..4 + bad.len() {
                server.serve_one().expect("serve");
            }
        });
        let (status, body) = request(addr, Some(&token), "GET", "/snapshot", "");
        assert_eq!(status, 200);
        let snapshot: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(snapshot["seq"], 3);
        assert_eq!(snapshot["state"], state);
        let page = |path: &str| {
            let (status, body) = request(addr, Some(&token), "GET", path, "");
            assert_eq!(status, 200, "{path}: {body}");
            serde_json::from_str::<serde_json::Value>(&body).unwrap()
        };
        let first = page("/ops?after=0&limit=2");
        assert_eq!(first["ops"].as_array().unwrap().len(), 2);
        assert_eq!(first["ops"][0]["seq"], 1);
        assert_eq!(
            first["ops"][0]["op"],
            serde_json::to_value(&ops[0]).unwrap()
        );
        assert_eq!(first["next"], 2);
        let last = page("/ops?after=2&limit=2");
        assert_eq!(last["ops"][0]["seq"], 3);
        assert!(last["next"].is_null());
        assert_eq!(page("/ops?after=3")["ops"], serde_json::json!([]));
        for path in bad {
            let (status, body) = request(addr, Some(&token), "GET", path, "");
            assert_eq!(status, 400, "{path}: {body}");
        }
        serving.join().expect("server thread");
    }

    /// Every endpoint but `/health` needs one of the server's tokens, and pushing needs
    /// write scope; the PWA's read token still reads.
    #[test]
    fn requests_need_a_token_and_pushing_needs_write_scope() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        super::super::db::migrate(&mut db).unwrap();
        let read = super::super::tokens::add(&db, "phone", Scope::Read).unwrap();
        let authority = Authority::load(db).unwrap();
        let mut server = SyncServer::bind("127.0.0.1:0".parse().unwrap(), authority).unwrap();
        let addr = server.local_addr();
        let serving = std::thread::spawn(move || {
            for _ in 0..7 {
                server.serve_one().expect("serve");
            }
        });
        let batch = r#"{"ops": []}"#;
        assert_eq!(request(addr, None, "GET", "/health", "").0, 200);
        assert_eq!(request(addr, None, "GET", "/snapshot", "").0, 401);
        assert_eq!(request(addr, None, "POST", "/ops", batch).0, 401);
        assert_eq!(
            request(addr, Some("tsk_forged"), "GET", "/snapshot", "").0,
            401
        );
        assert_eq!(request(addr, Some(&read), "GET", "/snapshot", "").0, 200);
        assert_eq!(request(addr, Some(&read), "GET", "/ops?after=0", "").0, 200);
        let (status, body) = request(addr, Some(&read), "POST", "/ops", batch);
        assert_eq!(status, 403, "{body}");
        serving.join().expect("server thread");
    }

    /// A request must name a protocol this server speaks, before anything else is checked;
    /// the bearer scheme is case-insensitive and one Authorization header is the rule.
    #[test]
    fn requests_name_a_protocol_the_server_speaks_and_one_bearer_token() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        super::super::db::migrate(&mut db).unwrap();
        let token = super::super::tokens::add(&db, "you", Scope::Write).unwrap();
        let authority = Authority::load(db).unwrap();
        let mut server = SyncServer::bind("127.0.0.1:0".parse().unwrap(), authority).unwrap();
        let addr = server.local_addr();
        let auth = format!("Authorization: Bearer {token}\r\n");
        let cases: Vec<(String, u16)> = vec![
            (auth.clone(), 426),
            (format!("X-Tsk-Protocol: 0\r\n{auth}"), 426),
            (format!("X-Tsk-Protocol: 2\r\n{auth}"), 426),
            (format!("X-Tsk-Protocol: one\r\n{auth}"), 426),
            (
                format!("X-Tsk-Protocol: 1\r\nX-Tsk-Protocol: 1\r\n{auth}"),
                426,
            ),
            ("X-Tsk-Protocol: 1\r\n".into(), 401),
            (format!("X-Tsk-Protocol: 1\r\n{auth}"), 200),
            (
                format!("X-Tsk-Protocol: 1\r\nAuthorization: bearer  {token}\r\n"),
                200,
            ),
            (format!("X-Tsk-Protocol: 1\r\n{auth}{auth}"), 400),
            (
                format!("X-Tsk-Protocol: 1\r\nAuthorization: Basic {token}\r\n"),
                401,
            ),
        ];
        let count = cases.len() + 1;
        let serving = std::thread::spawn(move || {
            for _ in 0..count {
                server.serve_one().expect("serve");
            }
        });
        let (status, body) = raw(addr, "", "GET", "/health", "");
        assert_eq!(status, 200, "health needs no protocol header");
        assert!(body.contains(r#""min_protocol_version":1"#), "{body}");
        for (headers, expected) in &cases {
            let (status, body) = raw(addr, headers, "GET", "/snapshot", "");
            assert_eq!(status, *expected, "{headers:?}: {body}");
            if status == 426 {
                let refusal: serde_json::Value = serde_json::from_str(&body).unwrap();
                assert_eq!(refusal["minimum"], 1);
                assert_eq!(refusal["server"], 1);
            }
        }
        serving.join().expect("server thread");
    }

    /// The last page says so even when it is exactly full; a strict query, and a client past
    /// the server's log (a reset store), refuse.
    #[test]
    fn paging_ends_on_a_full_last_page_and_refuses_loose_queries_and_a_client_ahead() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        super::super::db::migrate(&mut db).unwrap();
        let token = super::super::tokens::add(&db, "you", Scope::Write).unwrap();
        let mut authority = Authority::load(db).unwrap();
        let ops: Vec<Op> = (0..2)
            .map(|index| {
                Op::local(
                    "you",
                    Intent::Create {
                        task: uuid::Uuid::new_v4(),
                        title: format!("task {index}"),
                        notes: None,
                        scope: TaskScope::Global,
                        provenance: ProvenanceOrigin::Manual,
                        thread: None,
                        assignee: None,
                        base: None,
                        due: None,
                        after: Vec::new(),
                        steps: Vec::new(),
                    },
                )
            })
            .collect();
        authority.submit("you", &ops).unwrap();
        let mut server = SyncServer::bind("127.0.0.1:0".parse().unwrap(), authority).unwrap();
        let addr = server.local_addr();
        let refused = [
            "/ops?after=1&after=0",
            "/ops?after=+1",
            "/ops?after=-0",
            "/ops?&after=0",
            "/ops?after=0&",
            "/ops?after=0&limit=+2",
        ];
        let count = refused.len() + 3;
        let serving = std::thread::spawn(move || {
            for _ in 0..count {
                server.serve_one().expect("serve");
            }
        });
        let (status, body) = request(addr, Some(&token), "GET", "/ops?after=0&limit=2", "");
        assert_eq!(status, 200, "{body}");
        let page: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(page["ops"].as_array().unwrap().len(), 2);
        assert!(page["next"].is_null(), "the full page is the last: {body}");
        let (status, body) = request(addr, Some(&token), "GET", "/ops?after=0&limit=1", "");
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["next"],
            1
        );
        for path in refused {
            assert_eq!(
                request(addr, Some(&token), "GET", path, "").0,
                400,
                "{path}"
            );
        }
        let (status, body) = request(addr, Some(&token), "GET", "/ops?after=3", "");
        assert_eq!(status, 409, "{body}");
        serving.join().expect("server thread");
    }
}
