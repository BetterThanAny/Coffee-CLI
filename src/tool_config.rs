//! Per-tool user-configurable launch overrides.
//!
//! Lives at `~/.coffee-cli/tools.json`. Each entry overrides one or more
//! pieces of the built-in defaults for spawning a CLI tool. Empty fields
//! fall through to the built-in behavior — the user only specifies what
//! they want different.
//!
//! Schema:
//! ```json
//! {
//!   "claude": {
//!     "default_cwd": "/home/user/work",
//!     "history_path": ""
//!   }
//! }
//! ```
//!
//! - `command` and `extra_args` are kept in the on-disk schema for backward
//!   compatibility, but are ignored and rejected on write. Letting the
//!   frontend choose launch binaries or flags turns a renderer compromise
//!   into command execution.
//! - `default_cwd`: pre-fills the cwd selector when starting a new tab
//!   of this tool. Empty falls through to the launchpad's last-used cwd.
//! - `history_path`: directory containing this tool's session history
//!   files. Empty falls through to the built-in scan path. Used by the
//!   history board's per-tool collectors.
//!
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolConfigEntry {
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub extra_args: Vec<String>,
    #[serde(default)]
    pub default_cwd: String,
    #[serde(default)]
    pub history_path: String,
}

impl ToolConfigEntry {
    pub fn is_empty(&self) -> bool {
        self.command.is_empty()
            && self.extra_args.is_empty()
            && self.default_cwd.is_empty()
            && self.history_path.is_empty()
    }
}

pub type ToolConfig = HashMap<String, ToolConfigEntry>;

fn config_path() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".coffee-cli").join("tools.json"))
}

pub fn load() -> ToolConfig {
    let Some(p) = config_path() else {
        return HashMap::new();
    };
    let Ok(s) = std::fs::read_to_string(&p) else {
        return HashMap::new();
    };
    serde_json::from_str::<ToolConfig>(&s).unwrap_or_default()
}

pub fn save(cfg: &ToolConfig) -> std::io::Result<()> {
    let Some(p) = config_path() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "no home dir",
        ));
    };
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(cfg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    // Atomic write so an interrupted save can't leave the file
    // half-written and unparseable on the next launch.
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

pub fn get(tool: &str) -> ToolConfigEntry {
    let mut entry = load().remove(tool).unwrap_or_default();

    // Older configs may still contain command/argument overrides. Ignore them
    // on read as well as rejecting them on write; otherwise stale local config
    // could continue to hijack tool launches after an upgrade.
    entry.command.clear();
    entry.extra_args.clear();

    if contains_control(&entry.default_cwd) {
        entry.default_cwd.clear();
    }
    if contains_control(&entry.history_path) {
        entry.history_path.clear();
    }

    if !entry.default_cwd.trim().is_empty() {
        let valid = expand_path(&entry.default_cwd)
            .canonicalize()
            .map(|cwd| cwd.is_dir() && cwd.components().count() >= 3)
            .unwrap_or(false);
        if !valid {
            entry.default_cwd.clear();
        }
    }

    if !entry.history_path.trim().is_empty()
        && validated_history_path(tool, &entry.history_path).is_none()
    {
        entry.history_path.clear();
    }

    entry
}

pub fn set(tool: &str, entry: ToolConfigEntry) -> std::io::Result<()> {
    let mut cfg = load();
    if entry.is_empty() {
        cfg.remove(tool);
    } else {
        cfg.insert(tool.to_string(), entry);
    }
    save(&cfg)
}

fn allowed_tool(tool: &str) -> bool {
    matches!(tool, "claude" | "codex" | "terminal")
}

fn contains_control(s: &str) -> bool {
    s.chars().any(|c| c == '\0' || c == '\n' || c == '\r')
}

fn normal_components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str().map(|v| v.to_string()),
            _ => None,
        })
        .collect()
}

fn history_path_matches_tool(tool: &str, path: &Path) -> bool {
    let parts = normal_components(path);
    match tool {
        "claude" => parts.iter().any(|p| p == ".claude") && parts.iter().any(|p| p == "projects"),
        "codex" => parts.iter().any(|p| p == ".codex") && parts.iter().any(|p| p == "sessions"),
        _ => false,
    }
}

pub fn validated_history_path(tool: &str, input: &str) -> Option<PathBuf> {
    if input.trim().is_empty() || contains_control(input) {
        return None;
    }
    let expanded = expand_path(input);
    let canonical = expanded.canonicalize().ok()?;
    if canonical.components().count() < 3 {
        return None;
    }
    history_path_matches_tool(tool, &canonical).then_some(canonical)
}

pub fn validate_entry(tool: &str, entry: &ToolConfigEntry) -> Result<(), String> {
    if !allowed_tool(tool) {
        return Err(format!("Unsupported tool: {tool}"));
    }
    if !entry.command.trim().is_empty() {
        return Err("Command overrides are disabled for security".to_string());
    }
    if !entry.extra_args.is_empty() {
        return Err("Extra launch arguments are disabled for security".to_string());
    }
    if contains_control(&entry.default_cwd) || contains_control(&entry.history_path) {
        return Err("Configuration contains control characters".to_string());
    }
    if !entry.default_cwd.trim().is_empty() {
        let cwd = expand_path(&entry.default_cwd)
            .canonicalize()
            .map_err(|e| format!("Invalid default cwd: {e}"))?;
        if !cwd.is_dir() || cwd.components().count() < 3 {
            return Err("Default cwd must be a user project directory".to_string());
        }
    }
    if !entry.history_path.trim().is_empty()
        && validated_history_path(tool, &entry.history_path).is_none()
    {
        return Err("History path must point to that tool's session directory".to_string());
    }
    Ok(())
}

/// Expand a leading `~/` or bare `~` to the user's home directory.
/// Used by both `default_cwd` and `history_path` overrides since users
/// reasonably write `~/.claude/projects` and expect us to handle it.
/// Windows paths (`\\wsl.localhost\...` / `C:\...`) and absolute Unix
/// paths pass through unchanged.
pub fn expand_path(input: &str) -> std::path::PathBuf {
    if input == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
        return std::path::PathBuf::from("~");
    }
    if let Some(stripped) = input
        .strip_prefix("~/")
        .or_else(|| input.strip_prefix("~\\"))
    {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }
    std::path::PathBuf::from(input)
}

/// Resolve a tool's effective history-scan directory: user override
/// (with ~ expansion) if set, else `default` (caller-supplied).
pub fn history_path_for(tool: &str, default: std::path::PathBuf) -> std::path::PathBuf {
    let cfg = get(tool).history_path;
    validated_history_path(tool, &cfg).unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_path_handles_tilde_and_passthrough() {
        let home = dirs::home_dir().expect("test needs a home dir");
        // Tilde forms expand
        assert_eq!(expand_path("~"), home);
        assert_eq!(
            expand_path("~/.claude/projects"),
            home.join(".claude").join("projects")
        );
        // Backslash form on Windows-style paths
        assert_eq!(
            expand_path("~\\.claude\\projects"),
            home.join(".claude\\projects")
        );
        // Non-tilde paths pass through verbatim — UNC, absolute Unix, drive letters
        assert_eq!(
            expand_path("\\\\wsl.localhost\\Ubuntu\\home\\user\\.claude"),
            std::path::PathBuf::from("\\\\wsl.localhost\\Ubuntu\\home\\user\\.claude"),
        );
        assert_eq!(
            expand_path("/abs/unix/path"),
            std::path::PathBuf::from("/abs/unix/path")
        );
        assert_eq!(
            expand_path("C:\\Users\\someone"),
            std::path::PathBuf::from("C:\\Users\\someone")
        );
        // Tilde-in-middle does NOT expand (only leading)
        assert_eq!(
            expand_path("/foo/~/bar"),
            std::path::PathBuf::from("/foo/~/bar")
        );
    }

    #[test]
    fn empty_entry_check() {
        assert!(ToolConfigEntry::default().is_empty());
        assert!(!ToolConfigEntry {
            command: "x".into(),
            ..Default::default()
        }
        .is_empty());
        assert!(!ToolConfigEntry {
            extra_args: vec!["x".into()],
            ..Default::default()
        }
        .is_empty());
    }
}
