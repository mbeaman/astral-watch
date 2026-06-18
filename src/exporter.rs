//! Minimal HTTP exporter for Prometheus scrapes — `GET /metrics`.
//!
//! Hand-rolled over `std::net` to keep the dependency-light posture: one short-lived
//! connection at a time, bounded request size, a hard per-request deadline (so a client
//! dripping bytes cannot hold the sequential accept loop hostage), and no body parsing.
//! Serving is read-only against the [`Metrics`] cache; a scrape can never reach the
//! i2c bus. The listener is **opt-in** (`[export]` config or the `export` subcommand).

use crate::metrics::Metrics;
use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Cap on a single read/write syscall.
const IO_TIMEOUT: Duration = Duration::from_secs(2);
/// Hard ceiling on one request's lifetime — the accept loop is sequential, so no client
/// may hold it longer than this, no matter how slowly it feeds bytes.
const REQUEST_DEADLINE: Duration = Duration::from_secs(5);
/// Longest request head we accept.
const MAX_REQUEST: usize = 8 * 1024;

/// Bind `listen` and serve scrapes on a background thread. Returns the bound address
/// (useful with port 0). Binding errors fail fast at startup.
pub fn spawn(listen: &str, metrics: Arc<Metrics>) -> Result<SocketAddr> {
    let listener =
        TcpListener::bind(listen).with_context(|| format!("binding exporter on {listen}"))?;
    let addr = listener.local_addr()?;
    thread::Builder::new()
        .name("exporter".into())
        .spawn(move || {
            // serve sequentially; the per-request deadline bounds how long one client
            // can hold the loop. accept() errors (e.g. fd exhaustion) must not become
            // a silent busy-loop: warn once per streak and back off.
            let mut accept_failing = false;
            for stream in listener.incoming() {
                match stream {
                    Ok(sock) => {
                        accept_failing = false;
                        let _ = handle(sock, &metrics, REQUEST_DEADLINE);
                    }
                    Err(e) => {
                        if !accept_failing {
                            eprintln!("# exporter: accept failed: {e} (backing off, will retry)");
                            accept_failing = true;
                        }
                        thread::sleep(Duration::from_secs(1));
                    }
                }
            }
        })
        .context("spawning exporter thread")?;
    Ok(addr)
}

fn handle(mut sock: TcpStream, metrics: &Metrics, deadline: Duration) -> std::io::Result<()> {
    let started = Instant::now();
    sock.set_write_timeout(Some(IO_TIMEOUT))?;

    // read until end of headers (we never accept a body)
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        if buf.len() > MAX_REQUEST {
            return respond(
                &mut sock,
                "431 Request Header Fields Too Large",
                "text/plain",
                "",
            );
        }
        // each received byte must not reset the clock — enforce the overall deadline
        let remaining = deadline.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return respond(&mut sock, "408 Request Timeout", "text/plain", "");
        }
        sock.set_read_timeout(Some(remaining.min(IO_TIMEOUT)))?;
        let n = sock.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let head = String::from_utf8_lossy(&buf);
    let mut parts = head.lines().next().unwrap_or("").split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    // scrapers may append query params (scrape_config `params:`, cache busters)
    let path = target.split(['?', '#']).next().unwrap_or(target);

    match (method, path) {
        ("GET", "/metrics") => respond(
            &mut sock,
            "200 OK",
            "text/plain; version=0.0.4; charset=utf-8",
            &metrics.render(),
        ),
        ("GET", "/") => respond(
            &mut sock,
            "200 OK",
            "text/html; charset=utf-8",
            "<html><body>astral-watch — <a href=\"/metrics\">/metrics</a></body></html>\n",
        ),
        ("GET", _) => respond(&mut sock, "404 Not Found", "text/plain", "not found\n"),
        _ => respond(&mut sock, "405 Method Not Allowed", "text/plain", ""),
    }
}

fn respond(sock: &mut TcpStream, status: &str, ctype: &str, body: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    sock.write_all(head.as_bytes())?;
    sock.write_all(body.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::{Pin, Reading, PIN_COUNT};

    /// Plain-TcpStream client: immune to proxy env vars, no client dependency.
    fn get(addr: SocketAddr, path: &str) -> String {
        let mut sock = TcpStream::connect(addr).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(sock, "GET {path} HTTP/1.1\r\nhost: test\r\n\r\n").unwrap();
        let mut response = String::new();
        sock.read_to_string(&mut response).unwrap();
        response
    }

    #[test]
    fn serves_metrics_and_404s() {
        let metrics = Arc::new(Metrics::new());
        metrics.on_good_sample(&Reading {
            pins: [Pin {
                volts: 12.0,
                amps: 8.0,
            }; PIN_COUNT],
        });
        let addr = spawn("127.0.0.1:0", Arc::clone(&metrics)).unwrap();

        let response = get(addr, "/metrics");
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.contains("text/plain; version=0.0.4"), "{response}");
        assert!(
            response.contains("astral_watch_total_watts 576"),
            "{response}"
        );

        // Prometheus scrape_config `params:` / cache busters must still hit /metrics
        let response = get(addr, "/metrics?debug=1");
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");

        let response = get(addr, "/nope");
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");

        let response = get(addr, "/");
        assert!(response.contains("/metrics"), "{response}");
    }

    #[test]
    fn drip_feeding_client_hits_the_request_deadline() {
        // a client sending one byte at a time must be cut off at the overall deadline,
        // not granted a fresh timeout per byte (it would hold the sequential accept
        // loop — and every real scrape — hostage)
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let metrics = Metrics::new();
        let server = thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let started = Instant::now();
            let _ = handle(sock, &metrics, Duration::from_millis(400));
            started.elapsed()
        });

        let mut sock = TcpStream::connect(addr).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let dripper = thread::spawn(move || {
            // drip a byte every 100ms — never completing the request
            // keep dripping well past the assertion window, so finishing early can only
            // mean the request deadline fired (not that the client stopped sending)
            for _ in 0..80 {
                if sock.write_all(b"G").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
            let mut response = String::new();
            let _ = sock.read_to_string(&mut response);
            response
        });

        let held_for = server.join().unwrap();
        // deadline is 400ms; a generous 4s ceiling proves "bounded, not hung" without
        // flaking on a loaded CI runner, while the 8s dripper proves the deadline (not the
        // client) ended it
        assert!(
            held_for < Duration::from_secs(4),
            "drip client held the handler for {held_for:?}"
        );
        let response = dripper.join().unwrap();
        // the handler either FINs with the 408 body or, on a loaded runner, RSTs the socket
        // (unread drip bytes still in the recv buffer) so the client loses the body — both prove
        // the deadline dropped the slow client; held_for < 4s above already proved it fired
        assert!(
            response.is_empty() || response.contains("408"),
            "expected a 408 or a dropped connection, got: {response}"
        );
    }

    #[test]
    fn bad_listen_address_fails_fast() {
        assert!(spawn("definitely-not-an-address", Arc::new(Metrics::new())).is_err());
    }
}
