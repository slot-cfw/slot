//! A local HTTPS server for tests only (feature `fake-server`): the RetroAchievements fake
//! server of 0.6.0 Phase 3. It presents a certificate for the made-up host `ra.test` from a
//! throwaway test authority (`tests/data`), records every request, and answers with
//! whatever the test's handler returns (including stalls and dropped connections).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

pub const CA_PEM: &[u8] = include_bytes!("../tests/data/ca.pem");
const SERVER_PEM: &[u8] = include_bytes!("../tests/data/server.pem");
const SERVER_KEY: &[u8] = include_bytes!("../tests/data/server.key");
pub const HOST: &str = "ra.test";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// A form field from the query string or the url-encoded body.
    pub fn param(&self, name: &str) -> Option<String> {
        let query = self.path.split_once('?').map(|(_, q)| q).unwrap_or("");
        for part in query.split('&').chain(self.body.split('&')) {
            if let Some((k, v)) = part.split_once('=') {
                if k == name {
                    return Some(decode(v));
                }
            }
        }
        None
    }
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(v);
                    i += 2;
                } else {
                    out.push(b'%');
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What the server does with a request.
#[derive(Clone, Debug)]
pub enum Reply {
    Answer {
        status: u16,
        body: String,
    },
    /// Wait this long, then answer.
    Stall(Duration, u16, String),
    /// Close the connection without answering.
    Drop,
}

impl Reply {
    pub fn ok(body: &str) -> Reply {
        Reply::Answer {
            status: 200,
            body: body.to_string(),
        }
    }

    pub fn status(status: u16, body: &str) -> Reply {
        Reply::Answer {
            status,
            body: body.to_string(),
        }
    }
}

type Handler = Arc<dyn Fn(&Request) -> Reply + Send + Sync>;

pub struct Server {
    pub addr: SocketAddr,
    log: Arc<Mutex<Vec<Request>>>,
    handler: Arc<Mutex<Handler>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    pub fn start(handler: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(SERVER_PEM)
            .map(|c| c.unwrap())
            .collect();
        let key = PrivateKeyDer::from_pem_slice(SERVER_KEY).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = Arc::new(
            rustls::ServerConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(certs, key)
                .unwrap(),
        );
        let log = Arc::new(Mutex::new(Vec::new()));
        let handler: Arc<Mutex<Handler>> = Arc::new(Mutex::new(Arc::new(handler)));
        let stop = Arc::new(AtomicBool::new(false));
        let (l, h, s) = (log.clone(), handler.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((tcp, _)) => {
                        let (config, l, h) = (config.clone(), l.clone(), h.clone());
                        std::thread::spawn(move || serve(tcp, config, l, h));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(2)),
                }
            }
        });
        Server {
            addr,
            log,
            handler,
            stop,
            thread: Some(thread),
        }
    }

    /// Changes what the server answers from now on.
    pub fn set(&self, handler: impl Fn(&Request) -> Reply + Send + Sync + 'static) {
        *self.handler.lock().unwrap() = Arc::new(handler);
    }

    pub fn requests(&self) -> Vec<Request> {
        self.log.lock().unwrap().clone()
    }

    /// Requests of one `r=` kind (`login2`, `awardachievement`, ...).
    pub fn calls(&self, r: &str) -> Vec<Request> {
        self.requests()
            .into_iter()
            .filter(|q| q.param("r").as_deref() == Some(r))
            .collect()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn serve(
    tcp: TcpStream,
    config: Arc<rustls::ServerConfig>,
    log: Arc<Mutex<Vec<Request>>>,
    handler: Arc<Mutex<Handler>>,
) {
    let _ = tcp.set_nonblocking(false);
    let _ = tcp.set_read_timeout(Some(Duration::from_secs(10)));
    let Ok(conn) = rustls::ServerConnection::new(config) else {
        return;
    };
    let mut tls = rustls::StreamOwned::new(conn, tcp);
    let Some(req) = read_request(&mut tls) else {
        return;
    };
    log.lock().unwrap().push(req.clone());
    let h = handler.lock().unwrap().clone();
    let (status, body) = match h(&req) {
        Reply::Answer { status, body } => (status, body),
        Reply::Stall(d, status, body) => {
            std::thread::sleep(d);
            (status, body)
        }
        Reply::Drop => return,
    };
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        _ => "Status",
    };
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = tls.write_all(resp.as_bytes());
    let _ = tls.flush();
    tls.conn.send_close_notify();
    let _ = tls.flush();
}

fn read_request(s: &mut impl Read) -> Option<Request> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..i]).into_owned();
            let mut lines = head.lines();
            let first = lines.next()?;
            let mut parts = first.split_whitespace();
            let method = parts.next()?.to_string();
            let path = parts.next()?.to_string();
            let headers: Vec<(String, String)> = lines
                .filter_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    Some((k.trim().to_string(), v.trim().to_string()))
                })
                .collect();
            let len = headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, v)| v.parse::<usize>().ok())
                .unwrap_or(0);
            if buf.len() >= i + 4 + len {
                let body = String::from_utf8_lossy(&buf[i + 4..i + 4 + len]).into_owned();
                return Some(Request {
                    method,
                    path,
                    headers,
                    body,
                });
            }
        }
        match s.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}
