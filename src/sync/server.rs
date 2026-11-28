//! The HTTP side of `tsk-sync`: `GET /health` and `POST /ops`.

use std::io::Read;
use std::net::SocketAddr;

use serde::Deserialize;
use tiny_http::{Header, Method, Request, Response, Server};

use super::authority::{Authority, OpResult};
use crate::domain::Op;

/// The largest `POST /ops` body the server reads.
const MAX_BODY: u64 = 4 << 20;

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
                        "seq": self.authority.seq(),
                        "ops": ops,
                    }),
                ),
                Err(error) => failed(&error),
            },
            (Method::Post, "/ops") => self.post_ops(request),
            _ => text(404, "not found"),
        }
    }

    fn post_ops(&mut self, request: &mut Request) -> Reply {
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
        match self.authority.submit(&parsed.ops) {
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

    fn request(addr: SocketAddr, method: &str, path: &str, body: &str) -> (u16, String) {
        let mut stream = std::net::TcpStream::connect(addr).expect("connect");
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: tsk\r\nConnection: close\r\n\
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
        let (status, body) = request(addr, "POST", "/ops", &batch);
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
        let (status, body) = request(addr, "GET", "/health", "");
        assert_eq!(status, 200);
        assert_eq!(body, r#"{"ops":1,"protocol_version":1,"seq":1}"#);
        let (status, body) = request(addr, "POST", "/ops", r#"{"ops": [], "extra": 1}"#);
        assert_eq!(status, 400, "{body}");
        assert!(body.starts_with("not an ops batch"), "{body}");
        assert_eq!(request(addr, "POST", "/ops", "not json").0, 400);
        assert_eq!(request(addr, "GET", "/nope", "").0, 404);
        serving.join().expect("server thread");
    }
}
