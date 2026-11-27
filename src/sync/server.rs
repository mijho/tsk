//! The HTTP side of `tsk-sync`. The skeleton answers `GET /health`; the op endpoints come
//! with their tasks.

use std::net::SocketAddr;

use rusqlite::Connection;
use tiny_http::{Header, Method, Response, Server};

/// A bound server, ready to answer requests.
pub struct SyncServer {
    http: Server,
    db: Connection,
}

impl SyncServer {
    /// Bind `listen` (port 0 picks a free port) with an open, migrated database.
    pub fn bind(listen: SocketAddr, db: Connection) -> std::io::Result<Self> {
        let http = Server::http(listen).map_err(std::io::Error::other)?;
        Ok(Self { http, db })
    }

    /// The address actually bound.
    pub fn local_addr(&self) -> SocketAddr {
        self.http
            .server_addr()
            .to_ip()
            .expect("tsk-sync binds an IP address")
    }

    /// Answer requests until the listener closes.
    pub fn serve(&self) {
        for request in self.http.incoming_requests() {
            let response = self.respond(request.method(), request.url());
            let _ = request.respond(response);
        }
    }

    /// Answer one request, then return.
    pub fn serve_one(&self) -> std::io::Result<()> {
        let request = self.http.recv()?;
        let response = self.respond(request.method(), request.url());
        request.respond(response)
    }

    fn respond(&self, method: &Method, url: &str) -> Response<std::io::Cursor<Vec<u8>>> {
        let json = Header::from_bytes("Content-Type", "application/json").expect("header");
        match (method, url) {
            (Method::Get, "/health") => {
                let ops: Result<u64, _> =
                    self.db
                        .query_row("SELECT count(*) FROM ops", [], |row| row.get(0));
                match ops {
                    Ok(ops) => Response::from_string(format!(
                        r#"{{"protocol_version":{},"ops":{ops}}}"#,
                        super::PROTOCOL_VERSION
                    ))
                    .with_header(json),
                    Err(error) => Response::from_string(format!("database error: {error}"))
                        .with_status_code(500),
                }
            }
            _ => Response::from_string("not found").with_status_code(404),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::*;

    fn get(addr: SocketAddr, path: &str) -> String {
        let mut stream = std::net::TcpStream::connect(addr).expect("connect");
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: tsk\r\nConnection: close\r\n\r\n"
        )
        .expect("request");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("response");
        response
    }

    #[test]
    fn health_answers_with_the_protocol_version_and_others_are_not_found() {
        let mut db = Connection::open_in_memory().unwrap();
        super::super::db::migrate(&mut db).unwrap();
        let server = SyncServer::bind("127.0.0.1:0".parse().unwrap(), db).expect("bind");
        let addr = server.local_addr();
        let serving = std::thread::spawn(move || {
            server.serve_one().expect("health");
            server.serve_one().expect("missing");
        });
        let health = get(addr, "/health");
        assert!(health.starts_with("HTTP/1.1 200"), "{health}");
        assert!(
            health.ends_with(r#"{"protocol_version":1,"ops":0}"#),
            "{health}"
        );
        assert!(get(addr, "/ops").starts_with("HTTP/1.1 404"));
        serving.join().expect("server thread");
    }
}
