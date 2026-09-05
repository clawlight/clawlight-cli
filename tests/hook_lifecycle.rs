//! End-to-end tests of the hook backend: drive the real `clawlight` binary the
//! way Claude Code does — one lifecycle event as JSON on stdin — and observe
//! the resulting `~/.claude/clawlight/state.json`.
//!
//! Each test gets its own throwaway home directory. `dirs::home_dir()` follows
//! `$HOME` on unix, which is what sandboxes these; Windows resolves the profile
//! via the known-folder API (no env override), hence the cfg gate.
#![cfg(unix)]

use std::path::PathBuf;

use assert_cmd::Command;
use serde_json::{json, Value};
use tempfile::TempDir;

fn temp_home() -> TempDir {
    TempDir::new().expect("create temp home")
}

fn state_path(home: &TempDir) -> PathBuf {
    home.path()
        .join(".claude")
        .join("clawlight")
        .join("state.json")
}

/// Run `clawlight hook` inside the sandbox home with the given stdin.
fn run_hook(home: &TempDir, stdin: &str) -> assert_cmd::assert::Assert {
    Command::cargo_bin("clawlight")
        .expect("binary built")
        .arg("hook")
        .env("HOME", home.path())
        .env_remove("CLAWLIGHT_NAMING")
        .write_stdin(stdin.to_string())
        .assert()
}

/// A hook event as Claude Code sends it. `transcript_path` is deliberately
/// omitted: a `Stop` event carrying one would spawn the detached auto-namer,
/// which shells out to the `claude` CLI — the namer is tested separately
/// against a fake `claude` below.
fn event(name: &str, session_id: &str) -> String {
    json!({
        "hook_event_name": name,
        "session_id": session_id,
        "cwd": "/tmp/some-project",
    })
    .to_string()
}

fn read_state(home: &TempDir) -> Value {
    let content = std::fs::read_to_string(state_path(home)).expect("state.json exists");
    serde_json::from_str(&content).expect("state.json parses")
}

fn session_status(state: &Value, session_id: &str) -> String {
    state["sessions"][session_id]["status"]
        .as_str()
        .unwrap_or_else(|| panic!("session {session_id} has a status"))
        .to_string()
}

/// Seed a pre-existing state.json, bypassing the binary.
fn seed_state(home: &TempDir, state: &Value) {
    let path = state_path(home);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, state.to_string()).unwrap();
}

#[test]
fn session_start_records_an_active_session() {
    let home = temp_home();
    run_hook(&home, &event("SessionStart", "sess-1")).success();

    let state = read_state(&home);
    assert_eq!(session_status(&state, "sess-1"), "active");
    assert_eq!(
        state["sessions"]["sess-1"]["project_path"],
        "/tmp/some-project"
    );
}

#[test]
fn full_lifecycle_maps_events_to_statuses() {
    let home = temp_home();
    let steps = [
        ("SessionStart", "active"),
        ("UserPromptSubmit", "active"),
        ("Stop", "inactive"),
        ("Notification", "needs_help"),
        ("SessionEnd", "done"),
    ];
    for (hook_event, expected) in steps {
        run_hook(&home, &event(hook_event, "sess-life")).success();
        let state = read_state(&home);
        assert_eq!(
            session_status(&state, "sess-life"),
            expected,
            "after {hook_event}"
        );
    }
}

#[test]
fn idle_prompt_notification_is_ignored() {
    let home = temp_home();
    run_hook(&home, &event("SessionStart", "sess-idle")).success();

    let idle = json!({
        "hook_event_name": "Notification",
        "session_id": "sess-idle",
        "notification_type": "idle_prompt",
    });
    run_hook(&home, &idle.to_string()).success();

    // Still active — an idle nudge must not flip the session to needs-help.
    assert_eq!(session_status(&read_state(&home), "sess-idle"), "active");
}

/// A session parked on a help request, as a `permission_prompt` (or a login
/// prompt) leaves it.
fn seed_needs_help(home: &TempDir, session_id: &str) {
    seed_state(
        home,
        &json!({
            "sessions": {
                session_id: {
                    "status": "needs_help",
                    "last_updated": "2020-01-01T00:00:00Z",
                    "project_path": null,
                    "notification_type": "permission_prompt",
                    "name": "Fix the build",
                }
            }
        }),
    );
}

fn notification(session_id: &str, notification_type: &str) -> String {
    json!({
        "hook_event_name": "Notification",
        "session_id": session_id,
        "cwd": "/tmp/some-project",
        "notification_type": notification_type,
    })
    .to_string()
}

#[test]
fn informational_notifications_never_touch_status() {
    let home = temp_home();
    run_hook(&home, &event("SessionStart", "sess-info")).success();
    run_hook(&home, &event("Stop", "sess-info")).success();

    for kind in [
        "idle_prompt",
        "agent_completed",
        "quota_auto_resume_fired",
        "quota_auto_resume_stale",
        "quota_auto_resume_disabled",
    ] {
        run_hook(&home, &notification("sess-info", kind)).success();
        assert_eq!(
            session_status(&read_state(&home), "sess-info"),
            "inactive",
            "{kind} must not flip an idle session"
        );
    }
}

#[test]
fn help_requests_and_unknown_notification_types_go_red() {
    let home = temp_home();
    for kind in [
        "permission_prompt",
        "elicitation_dialog",
        "elicitation_url_dialog",
        "agent_needs_input",
        "some_future_type",
    ] {
        run_hook(&home, &event("SessionStart", "sess-red")).success();
        run_hook(&home, &notification("sess-red", kind)).success();
        let state = read_state(&home);
        assert_eq!(session_status(&state, "sess-red"), "needs_help", "{kind}");
        assert_eq!(state["sessions"]["sess-red"]["notification_type"], kind);
    }
}

#[test]
fn auth_success_clears_a_pending_red() {
    // The reported bug: a session that went red for a login stayed red after
    // the login succeeded, until the next prompt was typed.
    let home = temp_home();
    seed_needs_help(&home, "sess-login");

    run_hook(&home, &notification("sess-login", "auth_success")).success();

    let state = read_state(&home);
    assert_eq!(session_status(&state, "sess-login"), "active");
    assert_eq!(
        state["sessions"]["sess-login"]["notification_type"],
        Value::Null
    );
    assert_eq!(state["sessions"]["sess-login"]["name"], "Fix the build");
}

#[test]
fn elicitation_answers_clear_a_pending_red() {
    for kind in ["elicitation_response", "elicitation_complete"] {
        let home = temp_home();
        seed_needs_help(&home, "sess-elicit");
        run_hook(&home, &notification("sess-elicit", kind)).success();
        assert_eq!(
            session_status(&read_state(&home), "sess-elicit"),
            "active",
            "{kind}"
        );
    }
}

#[test]
fn resolving_notifications_never_activate_an_idle_or_unknown_session() {
    // `/login` typed at an idle prompt fires auth_success too: the session is
    // still idle, and must stay yellow rather than flip green.
    let home = temp_home();
    run_hook(&home, &event("SessionStart", "sess-idle-login")).success();
    run_hook(&home, &event("Stop", "sess-idle-login")).success();
    let before = read_state(&home);

    run_hook(&home, &notification("sess-idle-login", "auth_success")).success();
    assert_eq!(read_state(&home), before, "no write for an idle session");

    run_hook(&home, &notification("sess-never-seen", "auth_success")).success();
    assert!(
        read_state(&home)["sessions"]
            .get("sess-never-seen")
            .is_none(),
        "no ghost row for an unseen session"
    );
}

#[test]
fn post_tool_use_clears_a_pending_red_and_nothing_else() {
    // Approving a permission runs the tool; its completion is the first
    // event after the approval, and must clear the red even when no further
    // PreToolUse or Stop follows for a while.
    let home = temp_home();
    seed_needs_help(&home, "sess-approved");
    run_hook(&home, &event("PostToolUse", "sess-approved")).success();
    assert_eq!(
        session_status(&read_state(&home), "sess-approved"),
        "active"
    );

    // But a tool finishing on its own (a background task landing after Stop)
    // is not evidence the session is working again.
    run_hook(&home, &event("Stop", "sess-approved")).success();
    let before = read_state(&home);
    run_hook(&home, &event("PostToolUse", "sess-approved")).success();
    assert_eq!(read_state(&home), before);

    run_hook(&home, &event("PostToolUse", "sess-unseen")).success();
    assert!(read_state(&home)["sessions"].get("sess-unseen").is_none());
}

#[test]
fn pre_tool_use_skips_the_write_when_already_active() {
    let home = temp_home();
    let old_timestamp = "2020-01-01T00:00:00Z";
    seed_state(
        &home,
        &json!({
            "sessions": {
                "sess-hot": {
                    "status": "active",
                    "last_updated": old_timestamp,
                    "project_path": null,
                    "notification_type": null,
                    "name": null,
                }
            }
        }),
    );

    run_hook(&home, &event("PreToolUse", "sess-hot")).success();

    // The seeded timestamp surviving proves no write happened.
    let state = read_state(&home);
    assert_eq!(state["sessions"]["sess-hot"]["last_updated"], old_timestamp);
}

#[test]
fn pre_tool_use_reactivates_an_inactive_session() {
    let home = temp_home();
    seed_state(
        &home,
        &json!({
            "sessions": {
                "sess-cold": {
                    "status": "inactive",
                    "last_updated": "2020-01-01T00:00:00Z",
                    "project_path": null,
                    "notification_type": null,
                    "name": null,
                }
            }
        }),
    );

    run_hook(&home, &event("PreToolUse", "sess-cold")).success();

    let state = read_state(&home);
    assert_eq!(session_status(&state, "sess-cold"), "active");
    assert_ne!(
        state["sessions"]["sess-cold"]["last_updated"],
        "2020-01-01T00:00:00Z"
    );
}

#[test]
fn unknown_events_and_missing_session_ids_write_nothing() {
    let home = temp_home();
    run_hook(&home, &event("SomeFutureEvent", "sess-x")).success();
    run_hook(
        &home,
        &json!({"hook_event_name": "SessionStart"}).to_string(),
    )
    .success();

    assert!(!state_path(&home).exists());
}

#[test]
fn malformed_input_is_a_safe_noop() {
    let home = temp_home();
    run_hook(&home, "this is not json {{{").success();
    run_hook(&home, "").success();

    assert!(!state_path(&home).exists());
}

#[test]
fn a_corrupt_state_file_is_never_clobbered() {
    let home = temp_home();
    // Simulate a mid-write snapshot / unknown future schema on disk.
    let garbage = r#"{"sessions": {"other-session": "#;
    let path = state_path(&home);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, garbage).unwrap();

    run_hook(&home, &event("SessionStart", "sess-new")).success();

    // The hook must refuse to write rather than wipe other sessions' status.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), garbage);
}

#[test]
fn an_existing_name_survives_status_updates() {
    let home = temp_home();
    seed_state(
        &home,
        &json!({
            "sessions": {
                "sess-named": {
                    "status": "active",
                    "last_updated": "2020-01-01T00:00:00Z",
                    "project_path": null,
                    "notification_type": null,
                    "name": "Fix Auth Bug",
                }
            }
        }),
    );

    run_hook(&home, &event("Stop", "sess-named")).success();

    let state = read_state(&home);
    assert_eq!(session_status(&state, "sess-named"), "inactive");
    assert_eq!(state["sessions"]["sess-named"]["name"], "Fix Auth Bug");
}

#[test]
fn sessions_are_tracked_independently() {
    let home = temp_home();
    run_hook(&home, &event("SessionStart", "sess-a")).success();
    run_hook(&home, &event("SessionStart", "sess-b")).success();
    run_hook(&home, &event("Stop", "sess-b")).success();

    let state = read_state(&home);
    assert_eq!(session_status(&state, "sess-a"), "active");
    assert_eq!(session_status(&state, "sess-b"), "inactive");
}

#[test]
fn the_naming_guard_env_disables_tracking() {
    let home = temp_home();
    Command::cargo_bin("clawlight")
        .expect("binary built")
        .arg("hook")
        .env("HOME", home.path())
        .env("CLAWLIGHT_NAMING", "1")
        .write_stdin(event("SessionStart", "sess-nested"))
        .assert()
        .success();

    assert!(!state_path(&home).exists());
}

// ---------------------------------------------------------------------------
// Auto-namer (`clawlight name <id> <transcript>`), run against a fake `claude`
// CLI placed first on PATH so the test never touches the real one.
// ---------------------------------------------------------------------------

/// Install an executable `claude` stub in the sandbox and return the PATH to
/// run with. The stub must come first so a real `claude` is never invoked.
fn fake_claude(home: &TempDir, script_body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;

    let bin_dir = home.path().join("fake-bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let script = bin_dir.join("claude");
    std::fs::write(&script, format!("#!/bin/sh\n{script_body}\n")).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

fn write_transcript(home: &TempDir, first_prompt: &str) -> PathBuf {
    let path = home.path().join("transcript.jsonl");
    let lines = [
        json!({"type": "summary", "summary": "not a message"}).to_string(),
        json!({"message": {"role": "assistant", "content": "ignored"}}).to_string(),
        json!({"message": {"role": "user", "content": first_prompt}}).to_string(),
    ];
    std::fs::write(&path, lines.join("\n")).unwrap();
    path
}

fn run_namer(home: &TempDir, path_env: &str, transcript: &PathBuf) {
    Command::cargo_bin("clawlight")
        .expect("binary built")
        .arg("name")
        .arg("sess-unnamed")
        .arg(transcript)
        .env("HOME", home.path())
        .env("PATH", path_env)
        .assert()
        .success();
}

fn seed_unnamed_session(home: &TempDir) {
    seed_state(
        home,
        &json!({
            "sessions": {
                "sess-unnamed": {
                    "status": "inactive",
                    "last_updated": "2020-01-01T00:00:00Z",
                    "project_path": null,
                    "notification_type": null,
                    "name": null,
                }
            }
        }),
    );
}

#[test]
fn the_namer_titles_a_session_via_the_claude_cli() {
    let home = temp_home();
    seed_unnamed_session(&home);
    let path_env = fake_claude(&home, r#"echo "Refactor Login Flow""#);
    let transcript = write_transcript(&home, "please refactor the login flow");

    run_namer(&home, &path_env, &transcript);

    let state = read_state(&home);
    assert_eq!(
        state["sessions"]["sess-unnamed"]["name"],
        "Refactor Login Flow"
    );
}

#[test]
fn the_namer_writes_nothing_when_the_cli_fails() {
    let home = temp_home();
    seed_unnamed_session(&home);
    let path_env = fake_claude(&home, "exit 1");
    let transcript = write_transcript(&home, "some prompt");

    run_namer(&home, &path_env, &transcript);

    let state = read_state(&home);
    assert_eq!(state["sessions"]["sess-unnamed"]["name"], Value::Null);
}
