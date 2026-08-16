//! The one-time anonymous install ping.
//!
//! On a machine's *first* install, clawlight sends a single fixed-shape JSON
//! body to <https://clawlight.dev/api/install> recording which coding agents it
//! found. That is the one thing we can't learn any other way, and the thing that
//! decides which harnesses get built and maintained next.
//!
//! The rules that make this defensible, all enforced below:
//!
//! - **No identifier, ever.** No install id, no machine id, no hostname, no
//!   username, no paths, no session data, no counts. Every field is a
//!   fixed-vocabulary enum or a bool, so two pings are indistinguishable and
//!   nothing here can be linked back to a person or to another ping.
//! - **Once per machine.** A marker file records that the ping was sent, so the
//!   re-installs that happen on every upgrade (`clawlight install` is idempotent
//!   and routinely re-run) send nothing.
//! - **Never from the tray daemon.** [`maybe_report_install`] is called only
//!   from `install_hooks`, the foreground path. `first_run_setup_daemon` calls
//!   `register_hooks` directly and so never reaches here. A background process
//!   that phones home with no terminal to show a notice in is exactly the thing
//!   that costs users' trust.
//! - **The payload is printed before it's sent**, along with the opt-out.
//! - **Opt-outs are checked before anything is built or spawned**:
//!   `CLAWLIGHT_NO_TELEMETRY`, the cross-tool `DO_NOT_TRACK`, `--no-telemetry`,
//!   and `CI` (automation never pinged in the first place).
//!
//! `uninstall` sends nothing; there is no exit ping.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Where the ping goes. Overridable only for local testing of the endpoint.
const ENDPOINT: &str = "https://clawlight.dev/api/install";

/// Human-readable explanation printed alongside the payload.
const PRIVACY_URL: &str = "https://clawlight.dev/privacy";

/// Written once the ping has been attempted; its presence is what makes this
/// once-per-machine. Deliberately its own file rather than a field in
/// `config.json`: `maybe_autoenable_led` treats "no config.json yet" as the
/// clean-first-install signal, so writing config here would silently stop new
/// users' lamps from auto-enabling.
///
/// `uninstall` removes the whole `~/.claude/clawlight` directory, so a later
/// reinstall pings again, which is honest, since that is a fresh setup.
fn marker_path() -> PathBuf {
    crate::hook_dir().join(".install-reported")
}

/// Send the install ping if this is a first install and nothing opts out.
///
/// `opted_out_by_flag` carries `clawlight install --no-telemetry`. Best-effort
/// throughout: every failure path is a silent no-op, because nothing about
/// telemetry may ever break or slow down an install.
pub fn maybe_report_install(opted_out_by_flag: bool) {
    if opted_out_by_flag || opted_out_by_env() || marker_path().exists() {
        return;
    }

    let payload = payload();

    println!("\nOne-time anonymous ping about this install:");
    println!("  {payload}");
    println!("It carries no identifier of any kind, is sent once per machine, and");
    println!("tells us which coding agents to keep supporting. Nothing else is sent,");
    println!("ever: no paths, no prompts, no session data.");
    println!("Opt out: CLAWLIGHT_NO_TELEMETRY=1, or `clawlight install --no-telemetry`.");
    println!("Details: {PRIVACY_URL}");

    // Marked *before* sending: this must never send twice, and we deliberately
    // don't wait on the request (see `send`), so "attempted" is the only thing
    // we can honestly record. A ping lost to a flaky network is not retried.
    mark_reported();
    send(&payload);
}

/// True if the environment asks us not to send.
fn opted_out_by_env() -> bool {
    let truthy = |name: &str| is_truthy(std::env::var(name).ok().as_deref());
    truthy("CLAWLIGHT_NO_TELEMETRY")
        // The cross-tool standard (consoledonottrack.com); honoured on principle
        // even though most users will reach for the clawlight-specific one.
        || truthy("DO_NOT_TRACK")
        // Automation isn't a user and would badly skew "how many machines".
        || truthy("CI")
        // The auto-namer's nested `claude` call, which must stay inert.
        || truthy("CLAWLIGHT_NAMING")
}

/// Env-var truthiness: set and not one of the "off" spellings.
fn is_truthy(value: Option<&str>) -> bool {
    match value.map(str::trim) {
        None | Some("") | Some("0") => false,
        Some(v) => !v.eq_ignore_ascii_case("false"),
    }
}

/// Build the JSON body. Everything in here is a closed vocabulary; read the
/// whole function to confirm the payload can't carry anything else.
fn payload() -> String {
    serde_json::json!({
        "event": "install",
        "version": env!("CARGO_PKG_VERSION"),
        "os": os_name(),
        "arch": arch_name(),
        "harnesses": crate::harness::detected_harnesses(),
        "lamp": crate::led::detect_board().is_some(),
        "source": install_source(),
    })
    .to_string()
}

fn os_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "other"
    }
}

/// Coarse architecture. Only the three-value answer leaves the machine: no OS
/// version, no CPU model, nothing with enough entropy to narrow down a machine.
fn arch_name() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        _ => "other",
    }
}

/// How this binary got installed, inferred from where it lives. Only the
/// resulting enum is ever sent; the path itself never leaves the process.
fn install_source() -> &'static str {
    let Ok(exe) = std::env::current_exe() else {
        return "other";
    };
    classify_source(&exe.to_string_lossy().to_lowercase())
}

/// Split out from [`install_source`] so the mapping is testable without
/// depending on where the test binary happens to sit.
fn classify_source(lower_path: &str) -> &'static str {
    let has = |needle: &str| lower_path.contains(needle);
    if has("/cellar/") || has("/homebrew/") || has("/linuxbrew/") {
        "brew"
    } else if has("/.cargo/") || has("\\.cargo\\") {
        "cargo"
    } else if has("\\programs\\clawlight") {
        // %LOCALAPPDATA%\Programs\clawlight, where install.ps1 puts it.
        "script"
    } else {
        "other"
    }
}

fn mark_reported() {
    let path = marker_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(
        &path,
        "clawlight sent its one-time anonymous install ping; see \
         https://clawlight.dev/privacy\n",
    );
}

/// POST the payload via the system curl, the same approach `usage.rs` takes.
/// No HTTP stack is worth the dependency tree for two requests in the whole
/// binary.
///
/// Detached and never waited on: the install must not stall behind a slow or
/// captive network, so the child is orphaned to finish (or not) on its own with
/// a hard 5-second cap. The body goes in over stdin rather than argv so it stays
/// out of the process list.
fn send(payload: &str) {
    let endpoint = std::env::var("CLAWLIGHT_TELEMETRY_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| ENDPOINT.to_string());

    let mut cmd = Command::new("curl");
    cmd.args([
        "-sf",
        "--max-time",
        "5",
        "-X",
        "POST",
        "-H",
        "content-type: application/json",
        "--data-binary",
        "@-",
        &endpoint,
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    crate::spawn::configure_detached(&mut cmd);

    let Ok(mut child) = cmd.spawn() else {
        return; // No curl on this machine: drop it, never surface an error.
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truthiness_of_opt_out_vars() {
        assert!(is_truthy(Some("1")));
        assert!(is_truthy(Some("true")));
        assert!(is_truthy(Some("TRUE")));
        assert!(is_truthy(Some("yes")));
        // Unset, empty, and the explicit "off" spellings must not opt out; a
        // shell profile with `CI=` or `DO_NOT_TRACK=0` isn't asking for silence.
        assert!(!is_truthy(None));
        assert!(!is_truthy(Some("")));
        assert!(!is_truthy(Some("  ")));
        assert!(!is_truthy(Some("0")));
        assert!(!is_truthy(Some("false")));
        assert!(!is_truthy(Some("False")));
    }

    #[test]
    fn install_sources_classify() {
        assert_eq!(
            classify_source("/opt/homebrew/cellar/clawlight/0.13.0/bin/clawlight"),
            "brew"
        );
        assert_eq!(
            classify_source("/home/linuxbrew/.linuxbrew/bin/clawlight"),
            "brew"
        );
        assert_eq!(classify_source("/users/x/.cargo/bin/clawlight"), "cargo");
        assert_eq!(
            classify_source("c:\\users\\x\\.cargo\\bin\\clawlight.exe"),
            "cargo"
        );
        assert_eq!(
            classify_source("c:\\users\\x\\appdata\\local\\programs\\clawlight\\clawlight.exe"),
            "script"
        );
        assert_eq!(classify_source("/usr/local/bin/clawlight"), "other");
    }

    #[test]
    fn payload_carries_only_the_declared_fields() {
        // The guarantee users are given is that the ping is a handful of
        // low-cardinality facts. Pin the exact key set so no future edit can
        // widen it without this failing.
        let value: serde_json::Value = serde_json::from_str(&payload()).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "arch",
                "event",
                "harnesses",
                "lamp",
                "os",
                "source",
                "version"
            ]
        );
    }

    #[test]
    fn payload_leaks_no_paths_or_machine_identity() {
        let body = payload();
        // The two things most likely to sneak in via a careless edit: the
        // binary's own path, and the home directory it was installed from.
        if let Some(home) = dirs::home_dir().and_then(|h| h.to_str().map(str::to_string)) {
            assert!(
                !body.contains(&home),
                "payload leaked a home directory: {body}"
            );
        }
        // No declared field can contain a path separator, so their presence
        // would mean something path-shaped got in.
        assert!(!body.contains('/'), "payload looks path-like: {body}");
        assert!(
            !body.contains('\\'),
            "payload leaked a Windows path: {body}"
        );
    }

    #[test]
    fn payload_values_match_the_servers_vocabulary() {
        // app/api/install (claw-light-web) drops anything outside these sets, so
        // a drift here would silently stop the data arriving.
        let value: serde_json::Value = serde_json::from_str(&payload()).unwrap();
        assert_eq!(value["event"], "install");
        assert!(["macos", "windows", "linux", "other"].contains(&value["os"].as_str().unwrap()));
        assert!(["arm64", "x64", "other"].contains(&value["arch"].as_str().unwrap()));
        assert!(["brew", "cargo", "script", "other"].contains(&value["source"].as_str().unwrap()));
        assert!(value["lamp"].is_boolean());
        assert!(value["harnesses"].is_array());
        assert!(value["version"].as_str().unwrap().split('.').count() == 3);
    }
}
