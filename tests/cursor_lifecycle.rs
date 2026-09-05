//! Drive Cursor's documented native hook payloads through the built binary.
//! Unix-only: HOME isolates all state writes, as in the other lifecycle suites.
#![cfg(unix)]

use assert_cmd::Command;
use serde_json::{json, Value};
use tempfile::TempDir;

fn state_path(home: &TempDir) -> std::path::PathBuf {
    home.path().join(".claude/clawlight/state.json")
}

fn run(home: &TempDir, payload: Value) {
    Command::cargo_bin("clawlight")
        .unwrap()
        .arg("cursor-hook")
        .env("HOME", home.path())
        .write_stdin(payload.to_string())
        .assert()
        .success()
        .stdout("");
}

fn read(home: &TempDir) -> Value {
    serde_json::from_str(&std::fs::read_to_string(state_path(home)).unwrap()).unwrap()
}

fn payload(event: &str) -> Value {
    json!({
        "hook_event_name": event,
        "conversation_id": "cursor-conversation",
        "generation_id": "turn-one",
        "workspace_roots": ["/project", "/other-project"],
        "cursor_version": "2.0.0"
    })
}

#[test]
fn cursor_lifecycle_keeps_one_session_across_turns() {
    let home = TempDir::new().unwrap();
    for (event, status) in [
        ("sessionStart", "active"),
        ("beforeSubmitPrompt", "active"),
        ("preToolUse", "active"),
        ("postToolUse", "active"),
        ("stop", "inactive"),
        ("beforeSubmitPrompt", "active"),
        ("sessionEnd", "done"),
    ] {
        let mut p = payload(event);
        p["generation_id"] = json!(format!("turn-{event}"));
        p["cwd"] = json!("/project/subdirectory");
        // Lifecycle session_id is an alias; conversation_id is canonical.
        p["session_id"] = json!("must-not-split-the-conversation");
        run(&home, p);
        let state = read(&home);
        assert_eq!(state["sessions"].as_object().unwrap().len(), 1);
        let session = &state["sessions"]["cursor-conversation"];
        assert_eq!(session["status"], status);
        assert_eq!(session["harness"], "cursor");
        assert_eq!(session["project_path"], "/project");
    }
}

#[test]
fn first_prompt_names_once_and_thin_events_preserve_metadata() {
    let home = TempDir::new().unwrap();
    run(&home, payload("sessionStart"));
    let mut first = payload("beforeSubmitPrompt");
    let title = "修复🦀".repeat(30);
    first["prompt"] = json!(format!("  {title}\nsecond line"));
    run(&home, first);
    let name = read(&home)["sessions"]["cursor-conversation"]["name"].clone();
    assert!(name.as_str().unwrap().starts_with("修复🦀"));
    assert!(name.as_str().unwrap().chars().count() <= 51);
    run(
        &home,
        json!({"hook_event_name":"beforeSubmitPrompt",
        "conversation_id":"cursor-conversation", "prompt":"different prompt"}),
    );
    run(
        &home,
        json!({"hook_event_name":"stop", "conversation_id":"cursor-conversation"}),
    );
    let state = read(&home);
    let session = &state["sessions"]["cursor-conversation"];
    assert_eq!(session["name"], name);
    assert_eq!(session["project_path"], "/project");
    assert_eq!(session["status"], "inactive");
}

#[test]
fn lifecycle_alias_and_cwd_fallback_work() {
    let home = TempDir::new().unwrap();
    run(
        &home,
        json!({"hook_event_name":"sessionStart", "session_id":"alias",
        "workspace_roots":[], "cwd":"/cli-project"}),
    );
    run(
        &home,
        json!({"hook_event_name":"sessionEnd", "session_id":"alias"}),
    );
    let state = read(&home);
    assert_eq!(state["sessions"]["alias"]["status"], "done");
    assert_eq!(state["sessions"]["alias"]["project_path"], "/cli-project");
}

#[test]
fn irrelevant_and_invalid_events_do_not_create_sessions() {
    let home = TempDir::new().unwrap();
    for event in [
        "subagentStop",
        "afterTabFileEdit",
        "workspaceOpen",
        "futureEvent",
        "sessionEnd",
    ] {
        run(&home, payload(event));
    }
    for p in [
        Value::Null,
        json!({}),
        json!({"hook_event_name":"stop", "generation_id":"not-a-session"}),
        json!({"hook_event_name":"sessionStart", "conversation_id":" "}),
    ] {
        run(&home, p);
    }
    assert!(!state_path(&home).exists());
    run(&home, payload("beforeSubmitPrompt"));
    let before = std::fs::read(state_path(&home)).unwrap();
    run(&home, payload("subagentStop"));
    assert_eq!(std::fs::read(state_path(&home)).unwrap(), before);
}

#[test]
fn unreadable_state_is_never_replaced() {
    let home = TempDir::new().unwrap();
    let path = state_path(&home);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "unreadable state").unwrap();
    let mut p = payload("beforeSubmitPrompt");
    p["prompt"] = json!("name this session");
    run(&home, p);
    assert_eq!(std::fs::read_to_string(path).unwrap(), "unreadable state");
}
