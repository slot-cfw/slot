//! HTTPS to a public web service by name: RetroAchievements (0.6.0 Phase 3,
//! design/v6-ra-architecture.md section 10.2).
//!
//! Separate from the Syncthing client (`sync::https`), which keeps trusting only Syncthing's
//! own certificate. Here:
//! - the server is found by name (a DNS lookup on the calling thread, never the game or
//!   network thread) and proves it with a certificate for that name (SNI and host name
//!   verification) chaining to Mozilla's public roots (`webpki-roots`);
//! - certificate dates are checked against a time the caller gives (network time), so a
//!   handheld whose clock is wrong can still connect;
//! - every request has a total deadline (20 s) on top of the per-read limits, and the
//!   answer is capped (2 MB);
//! - HTTP/1.1, `Connection: close`, one request per connection.

use std::io::Write;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::client::WebPkiServerVerifier;
use rustls::crypto::ring;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::time_provider::TimeProvider;
use rustls::RootCertStore;

use crate::sync::https::{parse_response, read_response_capped, Error, Response};

pub const TOTAL_LIMIT: Duration = Duration::from_secs(20);
pub const CONNECT_LIMIT: Duration = Duration::from_secs(5);
pub const IO_LIMIT: Duration = Duration::from_secs(10);
pub const MAX_RESPONSE: usize = 2 << 20;

/// Which certificates to trust.
#[derive(Clone, Debug)]
pub enum Roots {
    /// Mozilla's public roots (production).
    Public,
    /// Only these (PEM): a test authority for the local fake server.
    Pem(Vec<u8>),
}

/// The time certificate dates are checked against.
pub type Clock = Arc<dyn Fn() -> Option<UnixTime> + Send + Sync>;

struct GivenTime(Clock);

impl std::fmt::Debug for GivenTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GivenTime")
    }
}

impl TimeProvider for GivenTime {
    fn current_time(&self) -> Option<UnixTime> {
        (self.0)()
    }
}

/// A public HTTPS service, by host name.
#[derive(Clone)]
pub struct Site {
    pub host: String,
    pub port: u16,
    pub roots: Roots,
    pub user_agent: String,
    /// Connect here instead of looking the name up (the local fake server).
    pub resolve_to: Option<SocketAddr>,
    pub total_limit: Duration,
    pub connect_limit: Duration,
    pub io_limit: Duration,
    pub max_response: usize,
}

impl Site {
    pub fn new(host: &str, user_agent: &str) -> Site {
        Site {
            host: host.to_string(),
            port: 443,
            roots: Roots::Public,
            user_agent: user_agent.to_string(),
            resolve_to: None,
            total_limit: TOTAL_LIMIT,
            connect_limit: CONNECT_LIMIT,
            io_limit: IO_LIMIT,
            max_response: MAX_RESPONSE,
        }
    }

    fn config(&self, clock: Clock) -> Result<Arc<rustls::ClientConfig>, Error> {
        let mut store = RootCertStore::empty();
        match &self.roots {
            Roots::Public => store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
            Roots::Pem(pem) => {
                for c in CertificateDer::pem_slice_iter(pem) {
                    let c = c.map_err(|e| Error::NoCertificate(e.to_string()))?;
                    store
                        .add(c)
                        .map_err(|e| Error::NoCertificate(e.to_string()))?;
                }
            }
        }
        let provider = Arc::new(ring::default_provider());
        let verifier =
            WebPkiServerVerifier::builder_with_provider(Arc::new(store), provider.clone())
                .build()
                .map_err(|e| Error::Tls(e.to_string()))?;
        let config =
            rustls::ClientConfig::builder_with_details(provider, Arc::new(GivenTime(clock)))
                .with_safe_default_protocol_versions()
                .map_err(|e| Error::Tls(e.to_string()))?
                .with_webpki_verifier(verifier)
                .with_no_client_auth();
        Ok(Arc::new(config))
    }

    /// The server's addresses. The lookup runs on a helper thread so the request can stop
    /// waiting at its deadline (`getaddrinfo` has no time limit of its own); a lookup still
    /// running then is left to finish on its own and its answer dropped.
    fn resolve(&self, deadline: Instant) -> Result<Vec<SocketAddr>, Error> {
        if let Some(a) = self.resolve_to {
            return Ok(vec![a]);
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let (host, port) = (self.host.clone(), self.port);
        std::thread::Builder::new()
            .name("slot-dns".into())
            .spawn(move || {
                let r = (host.as_str(), port)
                    .to_socket_addrs()
                    .map(|a| a.collect::<Vec<_>>());
                let _ = tx.send(r);
            })
            .map_err(|e| Error::Unreachable(format!("DNS: {e}")))?;
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Ok(addrs)) => Ok(addrs),
            Ok(Err(e)) => Err(Error::Unreachable(format!("DNS: {e}"))),
            Err(_) => Err(Error::Timeout),
        }
    }

    fn connect(&self, deadline: Instant) -> Result<TcpStream, Error> {
        let mut last = Error::Unreachable("no address".into());
        for addr in self.resolve(deadline)? {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::Timeout);
            }
            match TcpStream::connect_timeout(&addr, self.connect_limit.min(left)) {
                Ok(tcp) => {
                    let _ = tcp.set_nodelay(true);
                    return Ok(tcp);
                }
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => last = Error::Timeout,
                Err(e) => last = Error::Unreachable(e.to_string()),
            }
        }
        Err(last)
    }

    /// One request to `path_and_query` (starting with `/`). With `body`, a POST of that
    /// content type. `clock` gives the time certificates are checked against. The whole
    /// request, the name lookup, connection and TLS handshake included, ends by
    /// `total_limit`.
    pub fn request(
        &self,
        path_and_query: &str,
        body: Option<(&str, &str)>,
        clock: Clock,
    ) -> Result<Response, Error> {
        let deadline = Instant::now() + self.total_limit;
        let config = self.config(clock)?;
        let name = ServerName::try_from(self.host.clone())
            .map_err(|e| Error::Protocol(format!("host name: {e}")))?;
        let conn =
            rustls::ClientConnection::new(config, name).map_err(|e| Error::Tls(e.to_string()))?;
        let sock = Bounded {
            tcp: self.connect(deadline)?,
            deadline,
            io_limit: self.io_limit,
        };
        let mut tls = rustls::StreamOwned::new(conn, sock);
        let req = self.build(path_and_query, body);
        tls.write_all(req.as_bytes()).map_err(map_io)?;
        tls.flush().map_err(map_io)?;
        let raw = read_response_capped(&mut tls, self.max_response)?;
        parse_response(&raw)
    }

    fn build(&self, path: &str, body: Option<(&str, &str)>) -> String {
        let host = if self.port == 443 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        };
        let mut r = match body {
            Some(_) => format!("POST {path} HTTP/1.1\r\n"),
            None => format!("GET {path} HTTP/1.1\r\n"),
        };
        r.push_str(&format!(
            "Host: {host}\r\nUser-Agent: {}\r\nAccept: application/json\r\nConnection: close\r\n",
            self.user_agent
        ));
        match body {
            Some((content_type, b)) => r.push_str(&format!(
                "Content-Type: {content_type}\r\nContent-Length: {}\r\n\r\n{b}",
                b.len()
            )),
            None => r.push_str("\r\n"),
        }
        r
    }
}

/// The socket under TLS, with every read and write given only what is left of the total
/// deadline (so a server that answers a byte at a time, or a lossy link during the
/// handshake, cannot stretch the request).
struct Bounded {
    tcp: TcpStream,
    deadline: Instant,
    io_limit: Duration,
}

impl Bounded {
    fn arm(&self) -> std::io::Result<()> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "total deadline",
            ));
        }
        let t = Some(self.io_limit.min(left));
        self.tcp.set_read_timeout(t)?;
        self.tcp.set_write_timeout(t)
    }
}

impl std::io::Read for Bounded {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.arm()?;
        self.tcp.read(buf)
    }
}

impl Write for Bounded {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.arm()?;
        self.tcp.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.tcp.flush()
    }
}

fn map_io(e: std::io::Error) -> Error {
    match e.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => Error::Timeout,
        std::io::ErrorKind::InvalidData => Error::Tls(e.to_string()),
        _ => Error::Unreachable(e.to_string()),
    }
}

/// `UnixTime` from milliseconds since 1970 (None before 1970).
pub fn unix_time(ms: i64) -> Option<UnixTime> {
    (ms >= 0).then(|| UnixTime::since_unix_epoch(Duration::from_millis(ms as u64)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_server::{Reply, Server, CA_PEM, HOST};

    fn site(server: &Server) -> Site {
        let mut s = Site::new(HOST, "slot.advance/test (Linux aarch64) rcheevos/12.5.0");
        s.roots = Roots::Pem(CA_PEM.to_vec());
        s.resolve_to = Some(server.addr);
        s
    }

    fn now() -> Clock {
        Arc::new(|| {
            unix_time(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as i64,
            )
        })
    }

    #[test]
    fn a_post_reaches_the_named_host_with_its_user_agent_and_body() {
        let server = Server::start(|_| Reply::ok(r#"{"Success":true}"#));
        let r = site(&server)
            .request(
                "/dorequest.php",
                Some(("application/x-www-form-urlencoded", "r=login2&u=dean&t=x")),
                now(),
            )
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, r#"{"Success":true}"#);
        let q = &server.requests()[0];
        assert_eq!(q.method, "POST");
        assert_eq!(q.path, "/dorequest.php");
        assert_eq!(q.header("Host"), Some(HOST));
        assert_eq!(
            q.header("User-Agent"),
            Some("slot.advance/test (Linux aarch64) rcheevos/12.5.0")
        );
        assert_eq!(q.param("r").as_deref(), Some("login2"));
    }

    #[test]
    fn certificates_are_checked_against_the_given_time_not_the_system_clock() {
        let server = Server::start(|_| Reply::ok("{}"));
        // As if the handheld's clock said 1 January 2000: the certificate is not valid yet.
        let y2000: Clock = Arc::new(|| unix_time(946_684_800_000));
        assert!(matches!(
            site(&server).request("/", None, y2000),
            Err(Error::Tls(_))
        ));
        // Given network time instead, the same request succeeds.
        assert_eq!(site(&server).request("/", None, now()).unwrap().status, 200);
    }

    #[test]
    fn only_the_trusted_roots_and_the_right_name_are_accepted() {
        let server = Server::start(|_| Reply::ok("{}"));
        let mut public = site(&server);
        public.roots = Roots::Public;
        assert!(
            public.request("/", None, now()).is_err(),
            "a test certificate is never trusted with the public roots"
        );
        let mut wrong = site(&server);
        wrong.host = "retroachievements.org".into();
        assert!(
            wrong.request("/", None, now()).is_err(),
            "a certificate for another name is refused"
        );
        assert_eq!(server.requests().len(), 0, "nothing was sent to either");
    }

    #[test]
    fn the_whole_request_is_bounded_by_its_deadline() {
        let server = Server::start(|_| Reply::Stall(Duration::from_secs(3), 200, "{}".into()));
        let mut s = site(&server);
        s.total_limit = Duration::from_millis(500);
        let began = Instant::now();
        assert!(matches!(s.request("/", None, now()), Err(Error::Timeout)));
        assert!(
            began.elapsed() < Duration::from_millis(900),
            "{:?}",
            began.elapsed()
        );
    }

    #[test]
    fn a_server_that_stalls_the_handshake_or_drips_is_cut_off_at_the_deadline() {
        use std::io::Write as _;
        // Accepts the connection and never speaks TLS.
        let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = silent.local_addr().unwrap();
        std::thread::spawn(move || {
            let _held: Vec<_> = silent.incoming().take(1).collect();
            std::thread::sleep(Duration::from_secs(5));
        });
        let mut s = Site::new(HOST, "t");
        s.roots = Roots::Pem(CA_PEM.to_vec());
        s.resolve_to = Some(addr);
        s.total_limit = Duration::from_millis(600);
        let began = Instant::now();
        assert!(s.request("/", None, now()).is_err());
        assert!(
            began.elapsed() < Duration::from_millis(1000),
            "{:?}",
            began.elapsed()
        );

        // Speaks, but a byte every 100 ms: each read is short, the total is not.
        let drip = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = drip.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut c, _)) = drip.accept() {
                // A valid TLS record header announcing 16 KB of handshake, then its
                // contents a byte at a time: the client waits for the whole record.
                if c.write_all(&[0x16, 0x03, 0x03, 0x40, 0x00]).is_err() {
                    return;
                }
                for _ in 0..100 {
                    if c.write_all(&[0x00]).is_err() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        });
        s.resolve_to = Some(addr);
        let began = Instant::now();
        assert!(s.request("/", None, now()).is_err());
        assert!(
            began.elapsed() < Duration::from_millis(1000),
            "{:?}",
            began.elapsed()
        );
    }

    #[test]
    fn large_answers_and_dropped_connections_are_errors() {
        let big = "x".repeat(MAX_RESPONSE + 10);
        let server = Server::start(move |_| Reply::ok(&big));
        assert!(matches!(
            site(&server).request("/", None, now()),
            Err(Error::Protocol(_))
        ));
        let server = Server::start(|_| Reply::Drop);
        assert!(site(&server).request("/", None, now()).is_err());
    }
}
