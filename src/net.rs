//! `clawlight net` — Wi-Fi counterpart to the serial LED daemon: broadcasts
//! aggregate session state as a small JSON datagram on the local network so a
//! wireless ESP32 totem can sit anywhere in the house, no USB cable needed.
//!
//! Protocol: one UDP broadcast datagram per update, single-line JSON:
//!
//!   {"v":1,"agg":"R","red":1,"yellow":0,"green":2,
//!    "name":"Fix login bug","project":"clawlight-cli",
//!    "msg":"Claude needs your permission to use Bash"}
//!
//! `agg` matches the serial protocol letters (R/Y/G/N). `name`/`project`/`msg`
//! describe the highest-priority session — the one the aggregate color is
//! "about" — and `msg` carries the permission prompt text on needs-help
//! states, so a display can show *why* the light is red.
//!
//! Sent on every state change plus a periodic heartbeat; receivers should
//! treat a prolonged silence (say 10s) as "PC offline" rather than holding a
//! stale state forever.

use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::time::{Duration, Instant};

use crate::config;
use crate::state::{aggregate, read_hook_state, Aggregate, HookState, SessionStatus, Status};

pub const DEFAULT_PORT: u16 = 38737;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const HEARTBEAT: Duration = Duration::from_secs(2);
const RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// Longest `msg` we'll put on the wire — keeps the datagram well under one
/// MTU and is more than a small screen can show anyway.
const MSG_MAX: usize = 180;
const NAME_MAX: usize = 60;

fn agg_letter(agg: Aggregate) -> &'static str {
    match agg {
        Aggregate::Red => "R",
        Aggregate::Yellow => "Y",
        Aggregate::Green => "G",
        Aggregate::None => "N",
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        // Plain ASCII "..." — tiny OLED fonts don't have the U+2026 glyph.
        let cut: String = s.chars().take(max.saturating_sub(3)).collect();
        format!("{cut}...")
    }
}

/// The session the aggregate color is "about": any needs-help session wins,
/// then inactive (matching [`aggregate`]'s priority), then active; ties break
/// toward the most recently updated.
fn headline_session(state: &HookState) -> Option<&SessionStatus> {
    state
        .sessions
        .values()
        .filter(|s| s.status != Status::Done)
        .max_by_key(|s| {
            let priority = match s.status {
                Status::NeedsHelp => 3,
                Status::Inactive => 2,
                Status::Active => 1,
                Status::Done => 0,
            };
            (priority, s.last_updated.clone())
        })
}

/// Serialize the current state into the wire payload.
pub fn payload(state: &HookState) -> String {
    let mut red = 0u32;
    let mut yellow = 0u32;
    let mut green = 0u32;
    for s in state.sessions.values() {
        match s.status {
            Status::NeedsHelp => red += 1,
            Status::Inactive => yellow += 1,
            Status::Active => green += 1,
            Status::Done => {}
        }
    }

    let mut obj = serde_json::json!({
        "v": 1,
        "agg": agg_letter(aggregate(state)),
        "red": red,
        "yellow": yellow,
        "green": green,
    });

    if let Some(s) = headline_session(state) {
        let map = obj.as_object_mut().expect("payload is an object");
        if let Some(name) = &s.name {
            map.insert("name".into(), truncate(name, NAME_MAX).into());
        }
        if let Some(project) = &s.project_path {
            let base = std::path::Path::new(project)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| project.clone());
            map.insert("project".into(), truncate(&base, NAME_MAX).into());
        }
        if let Some(msg) = &s.message {
            map.insert("msg".into(), truncate(msg, MSG_MAX).into());
        }
    }

    obj.to_string()
}

/// The IP of the interface that carries the default route (the trick: a UDP
/// "connect" never sends a packet, but makes the OS pick the outgoing
/// interface). Binding the broadcast socket to this address matters on
/// machines with virtual adapters — WSL and Hyper-V NICs would otherwise
/// swallow the broadcast on some systems.
fn local_ip() -> IpAddr {
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:80")?;
            s.local_addr()
        })
        .map(|a| a.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
}

fn open_socket() -> std::io::Result<UdpSocket> {
    let socket = UdpSocket::bind((local_ip(), 0))?;
    socket.set_broadcast(true)?;
    Ok(socket)
}

/// Foreground command (`clawlight net`): broadcast state until killed. Handy
/// for debugging with a packet listener before the board is even flashed.
pub fn run(port_override: Option<u16>) -> anyhow::Result<()> {
    let port = port_override
        .or_else(|| config::read_config().net_port)
        .unwrap_or(DEFAULT_PORT);
    println!("clawlight net — broadcasting session state on UDP port {port}");

    loop {
        match open_socket() {
            Ok(socket) => {
                if let Ok(addr) = socket.local_addr() {
                    println!("Broadcasting from {addr}");
                }
                if let Err(e) = drive(&socket, port, || true, true) {
                    eprintln!("Broadcast failed ({e}); retrying...");
                }
            }
            Err(e) => eprintln!("Failed to open socket: {e}; retrying..."),
        }
        std::thread::sleep(RETRY_INTERVAL);
    }
}

/// Background driver for the menu bar daemon. Idles — sending nothing — while
/// the wireless setting is off; when on, broadcasts state until the setting is
/// turned off again. Never exits.
pub fn run_daemon() -> ! {
    loop {
        let cfg = config::read_config();
        if !cfg.net_enabled {
            std::thread::sleep(RETRY_INTERVAL);
            continue;
        }

        let port = cfg.net_port.unwrap_or(DEFAULT_PORT);
        if let Ok(socket) = open_socket() {
            let _ = drive(&socket, port, || config::read_config().net_enabled, false);
        }
        std::thread::sleep(RETRY_INTERVAL);
    }
}

/// Broadcast state until a send fails (typically the network went away) or
/// `keep_running` returns false, whichever comes first.
fn drive(
    socket: &UdpSocket,
    port: u16,
    keep_running: impl Fn() -> bool,
    verbose: bool,
) -> anyhow::Result<()> {
    let mut last_sent: Option<String> = None;
    let mut last_write = Instant::now();

    loop {
        if !keep_running() {
            return Ok(());
        }

        let body = payload(&read_hook_state());
        let changed = last_sent.as_deref() != Some(&body);

        if changed || last_write.elapsed() >= HEARTBEAT {
            socket.send_to(body.as_bytes(), (Ipv4Addr::BROADCAST, port))?;
            if changed && verbose {
                println!("state -> {body}");
            }
            last_sent = Some(body);
            last_write = Instant::now();
        }

        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{HookState, SessionStatus, Status};
    use std::collections::HashMap;

    fn session(status: Status, updated: &str, msg: Option<&str>) -> SessionStatus {
        SessionStatus {
            status,
            last_updated: updated.to_string(),
            project_path: Some("/home/ray/projects/clawlight-cli".to_string()),
            notification_type: None,
            name: Some("Test session".to_string()),
            message: msg.map(|s| s.to_string()),
        }
    }

    #[test]
    fn payload_prefers_needs_help_and_carries_message() {
        let mut sessions = HashMap::new();
        sessions.insert(
            "a".to_string(),
            session(Status::Active, "2026-07-01T10:00:00Z", None),
        );
        sessions.insert(
            "b".to_string(),
            session(
                Status::NeedsHelp,
                "2026-07-01T09:00:00Z",
                Some("Claude needs your permission to use Bash"),
            ),
        );
        let state = HookState { sessions };

        let v: serde_json::Value = serde_json::from_str(&payload(&state)).unwrap();
        assert_eq!(v["agg"], "R");
        assert_eq!(v["red"], 1);
        assert_eq!(v["green"], 1);
        assert_eq!(v["msg"], "Claude needs your permission to use Bash");
        assert_eq!(v["project"], "clawlight-cli");
    }

    #[test]
    fn payload_with_no_sessions_is_none_state() {
        let state = HookState {
            sessions: HashMap::new(),
        };
        let v: serde_json::Value = serde_json::from_str(&payload(&state)).unwrap();
        assert_eq!(v["agg"], "N");
        assert!(v.get("msg").is_none());
    }

    #[test]
    fn long_messages_are_truncated_for_the_wire() {
        let long = "x".repeat(500);
        let mut sessions = HashMap::new();
        sessions.insert(
            "a".to_string(),
            session(Status::NeedsHelp, "2026-07-01T10:00:00Z", Some(&long)),
        );
        let state = HookState { sessions };
        let v: serde_json::Value = serde_json::from_str(&payload(&state)).unwrap();
        assert!(v["msg"].as_str().unwrap().chars().count() <= MSG_MAX);
        assert!(payload(&state).len() < 1200, "must fit one datagram");
    }
}
