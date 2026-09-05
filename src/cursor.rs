//! Cursor native hooks: https://cursor.com/docs/hooks
//!
//! User hooks live in a shared `~/.cursor/hooks.json`. Merge only our command
//! entries, preserving foreign hooks and settings. Payloads identify their
//! event and conversation, so no plugin or transcript access is needed.

use std::path::{Path, PathBuf};

use anyhow::{ensure, Context};
use serde_json::{json, Value};

pub fn config_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".cursor"))
}

pub const HOOK_EVENTS: [&str; 6] = [
    "sessionStart",
    "beforeSubmitPrompt",
    "preToolUse",
    "postToolUse",
    "stop",
    "sessionEnd",
];

fn entry_is_ours(entry: &Value) -> bool {
    let Some(exe) = entry
        .get("command")
        .and_then(Value::as_str)
        .and_then(|cmd| cmd.trim().strip_suffix(" cursor-hook"))
    else {
        return false;
    };
    let exe = if let Some(quoted) = exe
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| exe.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
    {
        quoted
    } else if exe.chars().any(char::is_whitespace) {
        return false;
    } else {
        exe
    };
    matches!(
        exe.rsplit(['/', '\\']).next(),
        Some("clawlight" | "clawlight.exe")
    )
}

fn hook_command(exe: &Path) -> String {
    let path = exe.to_string_lossy();
    #[cfg(unix)]
    let quoted = format!("'{}'", path.replace('\'', "'\\''"));
    #[cfg(not(unix))]
    let quoted = format!("\"{path}\"");
    format!("{quoted} cursor-hook")
}

fn read_config(path: &Path) -> anyhow::Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(contents) => serde_json::from_str(&contents).context("Parsing Cursor hooks.json"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e).context("Reading Cursor hooks.json"),
    }
}

fn write_config(path: &Path, root: &Value) -> anyhow::Result<()> {
    let dir = path
        .parent()
        .context("Cursor hooks.json must have a parent")?;
    std::fs::create_dir_all(dir)?;
    let temp = dir.join(format!(".clawlight-cursor.{}.tmp", std::process::id()));
    std::fs::write(&temp, serde_json::to_string_pretty(root)? + "\n")?;
    std::fs::rename(temp, path).context("Writing Cursor hooks.json")
}

fn install_at(path: &Path, exe: &Path) -> anyhow::Result<()> {
    let mut root = read_config(path)?;
    let obj = root
        .as_object_mut()
        .context("Cursor hooks.json must be an object")?;
    ensure!(
        obj.get("version").is_none_or(|v| v == 1),
        "Unsupported Cursor hooks.json version; leaving it unchanged"
    );
    obj.entry("version").or_insert(json!(1));
    let hooks = obj
        .entry("hooks")
        .or_insert(json!({}))
        .as_object_mut()
        .context("Cursor hooks must be an object")?;
    // Validate before changing anything on disk, including malformed arrays
    // for events we don't currently register.
    ensure!(
        hooks.values().all(Value::is_array),
        "Cursor hook entries must be arrays"
    );
    hooks.retain(|_, entries| {
        let entries = entries.as_array_mut().unwrap();
        let had_ours = entries.iter().any(entry_is_ours);
        entries.retain(|entry| !entry_is_ours(entry));
        !had_ours || !entries.is_empty()
    });
    let command = hook_command(exe);
    for event in HOOK_EVENTS {
        let entries = hooks
            .entry(event)
            .or_insert(json!([]))
            .as_array_mut()
            .unwrap();
        entries.push(json!({ "command": command }));
    }
    write_config(path, &root)
}

pub fn install_hooks() -> anyhow::Result<()> {
    let path = config_dir()
        .context("No home directory")?
        .join("hooks.json");
    install_at(&path, &std::env::current_exe()?)?;
    println!("Wrote Cursor hooks to {}", path.display());
    println!("  (Cursor reloads hooks automatically; send a prompt to see a session)");
    Ok(())
}

fn uninstall_at(path: &Path) -> anyhow::Result<()> {
    let mut root = read_config(path)?;
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(());
    };
    let mut changed = false;
    hooks.retain(|_, entries| {
        let Some(entries) = entries.as_array_mut() else {
            return true;
        };
        let before = entries.len();
        entries.retain(|entry| !entry_is_ours(entry));
        let removed = before != entries.len();
        changed |= removed;
        !removed || !entries.is_empty()
    });
    if changed {
        write_config(path, &root)?;
        println!("Removed Cursor hooks from {}", path.display());
    }
    Ok(())
}

pub fn uninstall_hooks() {
    if let Some(path) = config_dir().map(|dir| dir.join("hooks.json")) {
        if let Err(e) = uninstall_at(&path) {
            eprintln!("clawlight: could not remove Cursor hooks: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn install_refreshes_ours_and_uninstall_preserves_foreign_configuration() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("hooks.json");
        let foreign = json!({
            "version": 1,
            "custom": {"enabled": true},
            "hooks": {
                "stop": [{"command": "./guard.sh", "loop_limit": 3}],
                "preToolUse": [{"command": "my-clawlight cursor-hook", "matcher": "Shell"}],
                "futureEvent": [{"command": "echo clawlight cursor-hook"}],
                "afterFileEdit": []
            }
        });
        let mut initial = foreign.clone();
        initial["hooks"]["stop"]
            .as_array_mut()
            .unwrap()
            .insert(0, json!({"command": "\"/old/clawlight\" cursor-hook"}));
        initial["hooks"]["obsolete"] = json!([{ "command": "clawlight cursor-hook" }]);
        std::fs::write(&path, initial.to_string()).unwrap();
        let exe = Path::new("/opt/claw light/clawlight");
        install_at(&path, exe).unwrap();
        let installed = std::fs::read_to_string(&path).unwrap();
        install_at(&path, exe).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), installed);
        let root = read_config(&path).unwrap();
        assert!(root["hooks"].get("obsolete").is_none());
        for event in HOOK_EVENTS {
            let entries = root["hooks"][event].as_array().unwrap();
            assert_eq!(entries.iter().filter(|e| entry_is_ours(e)).count(), 1);
            assert_eq!(entries.last().unwrap()["command"], hook_command(exe));
        }
        uninstall_at(&path).unwrap();
        assert_eq!(read_config(&path).unwrap(), foreign);
        let removed = std::fs::read_to_string(&path).unwrap();
        uninstall_at(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), removed);
    }

    #[test]
    fn invalid_or_future_config_is_never_overwritten() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("hooks.json");
        for content in [
            "{bad json",
            "[]",
            r#"{"version":2}"#,
            r#"{"hooks":null}"#,
            r#"{"hooks":{"stop":{}}}"#,
        ] {
            std::fs::write(&path, content).unwrap();
            assert!(install_at(&path, Path::new("clawlight")).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
        }
        std::fs::write(&path, "not json").unwrap();
        assert!(uninstall_at(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json");
    }

    #[test]
    fn fresh_install_and_absent_uninstall() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".cursor/hooks.json");
        uninstall_at(&path).unwrap();
        assert!(!path.exists());
        install_at(&path, Path::new("clawlight")).unwrap();
        assert_eq!(read_config(&path).unwrap()["version"], 1);
        uninstall_at(&path).unwrap();
        assert_eq!(read_config(&path).unwrap(), json!({"version":1,"hooks":{}}));
    }

    #[test]
    fn ownership_requires_the_exact_executable_and_subcommand() {
        for command in [
            "clawlight cursor-hook",
            "'/opt/claw light/clawlight' cursor-hook",
            r#""C:\Program Files\clawlight.exe" cursor-hook"#,
        ] {
            assert!(entry_is_ours(&json!({"command": command})), "{command}");
        }
        for command in [
            "my-clawlight cursor-hook",
            "echo clawlight cursor-hook",
            "clawlight cursor-hook-extra",
            "clawlight hook",
            "./guard cursor-hook",
        ] {
            assert!(!entry_is_ours(&json!({"command": command})), "{command}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn command_quotes_shell_metacharacters_in_install_paths() {
        let exe = Path::new("/tmp/it's $safe `here`/clawlight");
        let command = hook_command(exe);
        assert_eq!(
            command,
            "'/tmp/it'\\''s $safe `here`/clawlight' cursor-hook"
        );
        assert!(entry_is_ours(&json!({"command": command})));
    }
}
