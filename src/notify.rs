//! Delivering lifecycle events: ntfy push, generic JSON webhook, desktop (`notify-send`).
//!
//! Each transport gets its own worker thread and bounded queue, so a slow or dead
//! notification server can never stall sampling *or* starve the other transports.
//! Within a queue, raise messages are delivered first (a fresh OVERLOAD must not wait
//! behind stale resolves), each message gets a bounded number of delivery attempts, and
//! on overflow the oldest non-raise message is dropped. Failures degrade to deduplicated
//! stderr warnings; they never kill the watchdog. Messages still queued when the process
//! exits are lost — graceful-shutdown flushing needs signal handling (future arc).

use crate::config::{NotifyConfig, NtfyConfig, WebhookConfig};
use crate::lifecycle::{fmt_duration, Condition, Event};
use anyhow::{bail, Context, Result};
use std::collections::VecDeque;
use std::process::Command;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

/// Most messages a transport queue holds before dropping the oldest non-raise.
const QUEUE_CAP: usize = 64;
/// Delivery attempts per message before it is dropped.
const DELIVERY_ATTEMPTS: u32 = 3;
/// Pause between attempts on the worker thread.
const RETRY_PAUSE: Duration = Duration::from_secs(2);

/// How loudly a message should be delivered, mapped per transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Urgent,
    High,
    Default,
}

impl Priority {
    fn ntfy(self) -> &'static str {
        match self {
            Priority::Urgent => "urgent",
            Priority::High => "high",
            Priority::Default => "default",
        }
    }

    fn notify_send_urgency(self) -> &'static str {
        match self {
            Priority::Urgent => "critical",
            Priority::High => "normal",
            Priority::Default => "low",
        }
    }

    fn id(self) -> &'static str {
        match self {
            Priority::Urgent => "urgent",
            Priority::High => "high",
            Priority::Default => "default",
        }
    }
}

/// A rendered notification, ready for any transport.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    /// `raised` | `repeated` | `resolved`
    pub kind: &'static str,
    /// Stable condition id, e.g. `overload`.
    pub condition: &'static str,
    pub title: String,
    pub body: String,
    pub priority: Priority,
    pub ts: String,
}

/// Render a lifecycle event into a transport-agnostic message.
pub fn render(event: &Event, ts: &str) -> Message {
    match event {
        Event::Raised { condition, detail } => Message {
            kind: "raised",
            condition: condition.id(),
            title: format!("astral-watch: {}", condition.label()),
            body: detail.clone(),
            priority: priority_for(*condition),
            ts: ts.to_string(),
        },
        Event::Repeated {
            condition,
            detail,
            active_for,
        } => Message {
            kind: "repeated",
            condition: condition.id(),
            title: format!(
                "astral-watch: {} still active ({})",
                condition.label(),
                fmt_duration(*active_for)
            ),
            // a repeat is the backstop for a missed raise — it must page just as hard
            priority: priority_for(*condition),
            body: detail.clone(),
            ts: ts.to_string(),
        },
        Event::Resolved {
            condition,
            active_for,
        } => Message {
            kind: "resolved",
            condition: condition.id(),
            title: format!("astral-watch: {} resolved", condition.label()),
            body: format!("clear after {}", fmt_duration(*active_for)),
            priority: Priority::Default,
            ts: ts.to_string(),
        },
    }
}

fn priority_for(c: Condition) -> Priority {
    match c {
        // the two melt precursors page hardest
        Condition::Overload | Condition::Disconnected => Priority::Urgent,
        Condition::Imbalance | Condition::TelemetryLost => Priority::High,
    }
}

fn ntfy_tags(kind: &str) -> &'static str {
    match kind {
        "raised" => "rotating_light",
        "repeated" => "warning",
        _ => "white_check_mark",
    }
}

trait Transport: Send {
    fn name(&self) -> &'static str;
    fn deliver(&mut self, m: &Message) -> Result<()>;
}

fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into()
}

struct Ntfy {
    agent: ureq::Agent,
    cfg: NtfyConfig,
}

impl Transport for Ntfy {
    fn name(&self) -> &'static str {
        "ntfy"
    }

    fn deliver(&mut self, m: &Message) -> Result<()> {
        let url = format!("{}/{}", self.cfg.url.trim_end_matches('/'), self.cfg.topic);
        let mut req = self
            .agent
            .post(&url)
            .header("Title", &m.title)
            .header("Priority", m.priority.ntfy())
            .header("Tags", ntfy_tags(m.kind));
        if let Some(token) = &self.cfg.token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        req.send(m.body.as_str()).context("posting to ntfy")?;
        Ok(())
    }
}

struct Webhook {
    agent: ureq::Agent,
    cfg: WebhookConfig,
}

/// The JSON document a webhook receives.
fn webhook_payload(m: &Message) -> serde_json::Value {
    serde_json::json!({
        "source": "astral-watch",
        "event": m.kind,
        "condition": m.condition,
        "title": m.title,
        "detail": m.body,
        "priority": m.priority.id(),
        "timestamp": m.ts,
    })
}

impl Transport for Webhook {
    fn name(&self) -> &'static str {
        "webhook"
    }

    fn deliver(&mut self, m: &Message) -> Result<()> {
        let mut req = self.agent.post(&self.cfg.url);
        if let Some(token) = &self.cfg.token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        req.send_json(webhook_payload(m))
            .context("posting to webhook")?;
        Ok(())
    }
}

struct Desktop;

fn notify_send_args(m: &Message) -> Vec<String> {
    vec![
        "--app-name=astral-watch".into(),
        format!("--urgency={}", m.priority.notify_send_urgency()),
        m.title.clone(),
        m.body.clone(),
    ]
}

impl Transport for Desktop {
    fn name(&self) -> &'static str {
        "desktop"
    }

    fn deliver(&mut self, m: &Message) -> Result<()> {
        let status = Command::new("notify-send")
            .args(notify_send_args(m))
            .status()
            .context("running notify-send (is it installed, and is this a desktop session?)")?;
        if !status.success() {
            bail!("notify-send exited with {status}");
        }
        Ok(())
    }
}

struct ChannelState {
    queue: VecDeque<Message>,
    closed: bool,
}

struct Channel {
    state: Mutex<ChannelState>,
    cvar: Condvar,
}

/// Queue a message, dropping the oldest non-raise (raises last) once `cap` is reached.
fn enqueue(queue: &mut VecDeque<Message>, m: Message, cap: usize) {
    if queue.len() >= cap {
        match queue.iter().position(|x| x.kind != "raised") {
            Some(i) => {
                queue.remove(i);
            }
            None => {
                queue.pop_front();
            }
        }
    }
    queue.push_back(m);
}

/// Oldest raise first, then plain FIFO — a fresh alert never waits behind stale resolves.
fn next_message(queue: &mut VecDeque<Message>) -> Option<Message> {
    let i = queue.iter().position(|m| m.kind == "raised").unwrap_or(0);
    queue.remove(i)
}

fn worker(channel: Arc<Channel>, mut transport: Box<dyn Transport>) {
    let mut failing = false;
    loop {
        let m = {
            let mut st = channel.state.lock().unwrap();
            loop {
                if let Some(m) = next_message(&mut st.queue) {
                    break m;
                }
                if st.closed {
                    return;
                }
                st = channel.cvar.wait(st).unwrap();
            }
        };
        let mut result = Ok(());
        for attempt in 1..=DELIVERY_ATTEMPTS {
            result = transport.deliver(&m);
            if result.is_ok() {
                break;
            }
            if attempt < DELIVERY_ATTEMPTS {
                thread::sleep(RETRY_PAUSE);
            }
        }
        match result {
            Ok(()) => {
                if failing {
                    eprintln!("# notify: {} delivery recovered", transport.name());
                    failing = false;
                }
            }
            Err(e) => {
                if !failing {
                    eprintln!(
                        "# notify: {} delivery failed after {DELIVERY_ATTEMPTS} attempts: {e:#} (message dropped; later messages will still be attempted)",
                        transport.name()
                    );
                    failing = true;
                }
            }
        }
    }
}

/// Hands rendered messages to every configured transport, each on its own worker thread.
pub struct Dispatcher {
    channels: Vec<Arc<Channel>>,
}

impl Dispatcher {
    /// Build from config. Inert (no threads, `publish` is a no-op) when nothing is configured.
    pub fn from_config(cfg: &NotifyConfig) -> Self {
        let mut transports: Vec<Box<dyn Transport>> = Vec::new();
        if let Some(n) = &cfg.ntfy {
            transports.push(Box::new(Ntfy {
                agent: http_agent(),
                cfg: n.clone(),
            }));
        }
        if let Some(w) = &cfg.webhook {
            transports.push(Box::new(Webhook {
                agent: http_agent(),
                cfg: w.clone(),
            }));
        }
        if cfg.desktop {
            transports.push(Box::new(Desktop));
        }

        let channels = transports
            .into_iter()
            .map(|t| {
                let channel = Arc::new(Channel {
                    state: Mutex::new(ChannelState {
                        queue: VecDeque::new(),
                        closed: false,
                    }),
                    cvar: Condvar::new(),
                });
                let worker_channel = Arc::clone(&channel);
                thread::Builder::new()
                    .name(format!("notify-{}", t.name()))
                    .spawn(move || worker(worker_channel, t))
                    .expect("spawning notify thread");
                channel
            })
            .collect();
        Self { channels }
    }

    /// Queue a message for delivery on every transport. Never blocks the sampling loop.
    pub fn publish(&self, m: Message) {
        for channel in &self.channels {
            let mut st = channel.state.lock().unwrap();
            enqueue(&mut st.queue, m.clone(), QUEUE_CAP);
            channel.cvar.notify_one();
        }
    }
}

impl Drop for Dispatcher {
    fn drop(&mut self) {
        for channel in &self.channels {
            channel.state.lock().unwrap().closed = true;
            channel.cvar.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn raised() -> Event {
        Event::Raised {
            condition: Condition::Overload,
            detail: "OVERLOAD pins 1+2 >9.2A".into(),
        }
    }

    /// Like the production agent, but proxy env vars must not redirect loopback tests.
    fn test_agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .proxy(None)
            .build()
            .into()
    }

    #[test]
    fn render_maps_priorities_and_titles() {
        let m = render(&raised(), "2026-06-11T10:00:00");
        assert_eq!(m.kind, "raised");
        assert_eq!(m.condition, "overload");
        assert_eq!(m.title, "astral-watch: OVERLOAD");
        assert_eq!(m.priority, Priority::Urgent);

        // a repeat of a melt precursor must page as hard as the raise it backstops
        let repeated = Event::Repeated {
            condition: Condition::Overload,
            detail: "OVERLOAD pins 1+2 >9.2A".into(),
            active_for: Duration::from_secs(600),
        };
        assert_eq!(render(&repeated, "t").priority, Priority::Urgent);

        let resolved = Event::Resolved {
            condition: Condition::Imbalance,
            active_for: Duration::from_secs(120),
        };
        let m = render(&resolved, "2026-06-11T10:02:00");
        assert_eq!(m.priority, Priority::Default);
        assert_eq!(m.body, "clear after 2m00s");
    }

    #[test]
    fn webhook_payload_has_stable_schema() {
        let m = render(&raised(), "2026-06-11T10:00:00");
        let v = webhook_payload(&m);
        assert_eq!(v["source"], "astral-watch");
        assert_eq!(v["event"], "raised");
        assert_eq!(v["condition"], "overload");
        assert_eq!(v["priority"], "urgent");
        assert_eq!(v["timestamp"], "2026-06-11T10:00:00");
    }

    #[test]
    fn notify_send_args_map_urgency() {
        let args = notify_send_args(&render(&raised(), "t"));
        assert!(args.contains(&"--urgency=critical".to_string()));
        assert_eq!(args.last().unwrap(), "OVERLOAD pins 1+2 >9.2A");
    }

    fn msg(kind: &'static str, n: usize) -> Message {
        Message {
            kind,
            condition: "overload",
            title: format!("m{n}"),
            body: String::new(),
            priority: Priority::Default,
            ts: String::new(),
        }
    }

    #[test]
    fn raises_jump_the_queue() {
        let mut q = VecDeque::new();
        enqueue(&mut q, msg("resolved", 1), QUEUE_CAP);
        enqueue(&mut q, msg("repeated", 2), QUEUE_CAP);
        enqueue(&mut q, msg("raised", 3), QUEUE_CAP);
        enqueue(&mut q, msg("raised", 4), QUEUE_CAP);
        assert_eq!(
            next_message(&mut q).unwrap().title,
            "m3",
            "oldest raise first"
        );
        assert_eq!(next_message(&mut q).unwrap().title, "m4");
        assert_eq!(next_message(&mut q).unwrap().title, "m1", "then FIFO");
        assert_eq!(next_message(&mut q).unwrap().title, "m2");
        assert!(next_message(&mut q).is_none());
    }

    #[test]
    fn overflow_drops_oldest_non_raise_and_keeps_raises() {
        let mut q = VecDeque::new();
        enqueue(&mut q, msg("raised", 0), 3);
        enqueue(&mut q, msg("resolved", 1), 3);
        enqueue(&mut q, msg("repeated", 2), 3);
        enqueue(&mut q, msg("resolved", 3), 3); // drops m1
        assert_eq!(q.len(), 3);
        assert!(q.iter().all(|m| m.title != "m1"), "{q:?}");
        // all-raise queue: oldest raise goes
        let mut q: VecDeque<Message> = VecDeque::new();
        for n in 0..4 {
            enqueue(&mut q, msg("raised", n), 3);
        }
        assert_eq!(q.len(), 3);
        assert!(q.iter().all(|m| m.title != "m0"), "{q:?}");
    }

    /// Accept one HTTP request, return 200, and hand back the raw request text.
    fn one_shot_server() -> (u16, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let header_end = loop {
                let n = sock.read(&mut chunk).unwrap();
                assert!(n > 0, "peer closed before request completed");
                buf.extend_from_slice(&chunk[..n]);
                if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break p + 4;
                }
            };
            let head = String::from_utf8_lossy(&buf[..header_end]).to_lowercase();
            let content_length: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .map(|v| v.trim().parse().unwrap())
                .unwrap_or(0);
            while buf.len() < header_end + content_length {
                let n = sock.read(&mut chunk).unwrap();
                assert!(n > 0, "peer closed mid-body");
                buf.extend_from_slice(&chunk[..n]);
            }
            sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .unwrap();
            String::from_utf8_lossy(&buf).into_owned()
        });
        (port, handle)
    }

    #[test]
    fn ntfy_posts_topic_headers_and_body() {
        let (port, server) = one_shot_server();
        let mut t = Ntfy {
            agent: test_agent(),
            cfg: NtfyConfig {
                url: format!("http://127.0.0.1:{port}"),
                topic: "gpu-alerts".into(),
                token: Some("tk_secret".into()),
            },
        };
        t.deliver(&render(&raised(), "2026-06-11T10:00:00"))
            .unwrap();
        let req = server.join().unwrap();
        let lower = req.to_lowercase();
        assert!(req.starts_with("POST /gpu-alerts HTTP/1.1"), "{req}");
        assert!(lower.contains("title: astral-watch: overload"), "{req}");
        assert!(lower.contains("priority: urgent"), "{req}");
        assert!(lower.contains("authorization: bearer tk_secret"), "{req}");
        assert!(req.ends_with("OVERLOAD pins 1+2 >9.2A"), "{req}");
    }

    #[test]
    fn webhook_posts_json() {
        let (port, server) = one_shot_server();
        let mut t = Webhook {
            agent: test_agent(),
            cfg: WebhookConfig {
                url: format!("http://127.0.0.1:{port}/hook"),
                token: None,
            },
        };
        t.deliver(&render(&raised(), "2026-06-11T10:00:00"))
            .unwrap();
        let req = server.join().unwrap();
        assert!(req.starts_with("POST /hook HTTP/1.1"), "{req}");
        assert!(
            req.to_lowercase()
                .contains("content-type: application/json"),
            "{req}"
        );
        let body = &req[req.find("\r\n\r\n").unwrap() + 4..];
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(v["event"], "raised");
        assert_eq!(v["detail"], "OVERLOAD pins 1+2 >9.2A");
    }

    #[test]
    fn http_error_status_is_a_delivery_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut chunk = [0u8; 4096];
            let _ = sock.read(&mut chunk);
            sock.write_all(b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\n\r\n")
                .unwrap();
        });
        let mut t = Ntfy {
            agent: test_agent(),
            cfg: NtfyConfig {
                url: format!("http://127.0.0.1:{port}"),
                topic: "t".into(),
                token: None,
            },
        };
        assert!(t.deliver(&render(&raised(), "ts")).is_err());
    }
}
