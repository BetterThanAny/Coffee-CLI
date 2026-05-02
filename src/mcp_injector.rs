//! Per-pane MCP wiring for multi-agent mode.
//!
//! Each multi-agent pane gets:
//!   - a private temp dir at `<temp>/coffee-cli/panes/<sanitized-pane-id>/`
//!     holding the per-pane CLI artifacts (Claude mcp.json / Codex
//!     instructions.md)
//!   - a per-pane MCP HTTP server (with `self_pane_id` baked in at spawn
//!     time), independently of CLI kind. So `whoami()`, `list_panes()`'s
//!     `is_self`, and `[From <id>]` auto-prefixing in `send_to_pane()` are
//!     deterministic across all CLIs — no LLM guessing of pane identity
//!     even when 4 panes run the same CLI type.
//!
//! Per-CLI handoff (consumed by `server::tier_terminal_start_blocking`):
//!
//! | CLI    | Coffee CLI passes via …                                    | Pane reads from …                                         |
//! |--------|------------------------------------------------------------|-----------------------------------------------------------|
//! | Claude | `--mcp-config <pane-temp>/claude-mcp.json`                 | that JSON file                                            |
//! | Codex  | `-c mcp_servers.coffee-cli.url='<url>'`                    | command-line override (no file)                           |
//! |        | `-c model_instructions_file='<pane-temp>/inst.md'`         | per-pane temp file (no workspace touch)                   |
//!
//! Workspace pollution: zero. No `.md`, no `settings.json`, no
//! `mcp_servers` block ever lands in the user's project directory.
//!
//! Global pollution: zero. Claude and Codex are wired purely through
//! command-line flags plus OS temp files.
//!
//! Auth safety: we never set `CODEX_HOME`, so Codex's `~/.codex/auth.json`
//! remains reachable. Codex `-c` overrides merge onto the user's
//! `~/.codex/config.toml` rather than replacing it. User customisation
//! and credentials are preserved.
//!
//! Lifecycle: `prune_pane_artifacts()` is called once at app start so
//! the previous run's leftover dirs go away, again at shutdown for
//! belt-and-suspenders, and (for the per-tab subset) when a multi-agent
//! tab unmounts. New artifacts are created lazily in
//! `prepare_pane_config_dir()` on every PTY spawn — content is rewritten
//! idempotently each time, safe to call repeatedly for the same pane id.

use std::{fs, path::PathBuf};

use crate::mcp_server::McpEndpoint;

/// Key used for the Coffee CLI entry in every per-pane CLI config.
pub const MCP_KEY: &str = "coffee-cli";

/// Output of [`prepare_pane_config_dir`]. The caller picks the right
/// field based on CLI kind. Default-empty when `cli_kind` doesn't
/// match a multi-agent CLI.
#[derive(Debug, Clone, Default)]
pub struct PaneConfigPaths {
    /// `cli_kind == "claude"` only. Pass via `--mcp-config <path>`.
    pub claude_mcp_config_path: Option<PathBuf>,
    /// `cli_kind == "codex"` only. Caller appends these straight onto
    /// the codex argv (already in `-c key=value` pairs, ready to spawn).
    pub codex_extra_args: Vec<String>,
}

/// Build per-pane CLI artifacts for `pane_id` running `cli_kind`,
/// pointed at `endpoint`. `protocol_text` is written into the CLI's
/// instructions file (Codex `instructions.md`). Claude takes its
/// protocol text via `--append-system-prompt` and
/// doesn't read a file here — caller passes the same `protocol_text`
/// through that flag separately.
///
/// Idempotent: re-invoking with the same args overwrites in place.
/// Unknown `cli_kind` returns the default empty `PaneConfigPaths`.
pub fn prepare_pane_config_dir(
    pane_id: &str,
    cli_kind: &str,
    endpoint: &McpEndpoint,
    protocol_text: &str,
) -> std::io::Result<PaneConfigPaths> {
    let dir = panes_root().join(sanitize_pane_id(pane_id));
    fs::create_dir_all(&dir)?;

    let mut out = PaneConfigPaths::default();
    match cli_kind {
        "claude" => {
            let p = dir.join("claude-mcp.json");
            fs::write(&p, claude_mcp_json(endpoint))?;
            out.claude_mcp_config_path = Some(p);
        }
        "codex" => {
            // Per-pane protocol text. Referenced by `-c
            // model_instructions_file=<path>` so Codex bakes it into
            // the model's session context. No workspace touch.
            //
            // Note on the key name: Codex 0.x exposed this as
            // `experimental_instructions_file`, but starting with the
            // 2026-04 release the `experimental_` prefix is deprecated
            // and silently ignored — Codex prints
            //   `experimental_instructions_file is deprecated and ignored.
            //    Use model_instructions_file instead.`
            // and our protocol injection becomes a no-op (the multi-agent
            // CLI then has no idea how to call send_to_pane). Use the
            // new key. Older Codex versions just don't recognise it and
            // emit a soft warning, which is the strictly better failure
            // mode (warning + still-runnable shell vs silent no-op).
            let inst = dir.join("instructions.md");
            fs::write(&inst, protocol_text)?;
            // Codex `-c key=value` parses `value` as a TOML scalar. Use
            // TOML literal-strings ('...') so Windows backslashes in
            // the temp path don't accidentally trigger TOML escape
            // sequences (e.g. `\U` would otherwise look like a unicode
            // escape leadin in a basic-string).
            out.codex_extra_args = vec![
                "-c".to_string(),
                format!(
                    "mcp_servers.{key}.url='{url}'",
                    key = MCP_KEY,
                    url = endpoint.url
                ),
                "-c".to_string(),
                format!("model_instructions_file='{path}'", path = inst.display()),
            ];
        }
        _ => {}
    }
    Ok(out)
}

/// Wipe per-pane artifacts from any previous Coffee CLI run:
///   - `<temp>/coffee-cli/panes/`
///
/// Called once at app start (recover from crash residue), once at app
/// shutdown (tidy exit). Best-effort — missing dirs and permission
/// glitches are logged but never returned as errors. New artifacts get
/// recreated lazily by `prepare_pane_config_dir()` as panes spawn.
pub fn prune_pane_artifacts() {
    let root = panes_root();
    if root.exists() {
        if let Err(e) = fs::remove_dir_all(&root) {
            log::warn!(
                "[mcp-inject] prune {} failed: {} (will recreate per-pane dirs lazily)",
                root.display(),
                e
            );
        }
    }
}

fn panes_root() -> PathBuf {
    std::env::temp_dir().join("coffee-cli").join("panes")
}

/// Pane ids contain `::` and `/` which are unfriendly for filenames
/// on Windows. Replace anything outside `[A-Za-z0-9_-]` with `_`.
fn sanitize_pane_id(pane_id: &str) -> String {
    pane_id
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => c,
            _ => '_',
        })
        .collect()
}

fn claude_mcp_json(endpoint: &McpEndpoint) -> String {
    let body = serde_json::json!({
        "mcpServers": {
            MCP_KEY: {
                "type": "http",
                "url": endpoint.url,
            }
        }
    });
    serde_json::to_string_pretty(&body).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep() -> McpEndpoint {
        McpEndpoint {
            url: "http://127.0.0.1:50000/mcp".into(),
            port: 50000,
            pid: std::process::id(),
            started_at: 1_700_000_000,
        }
    }

    fn unique_pane(label: &str) -> String {
        format!("test::pane-{}-{}", label, std::process::id())
    }

    #[test]
    fn claude_writes_mcp_json_with_url() {
        let pid = unique_pane("claude");
        let out = prepare_pane_config_dir(&pid, "claude", &ep(), "PROMPT").unwrap();
        let p = out.claude_mcp_config_path.expect("claude returns path");
        let body = fs::read_to_string(&p).unwrap();
        assert!(body.contains("coffee-cli"));
        assert!(body.contains("http://127.0.0.1:50000/mcp"));
        let _ = fs::remove_dir_all(panes_root().join(sanitize_pane_id(&pid)));
    }

    #[test]
    fn codex_returns_minus_c_args_only() {
        let pid = unique_pane("codex");
        let out = prepare_pane_config_dir(&pid, "codex", &ep(), "PROTOCOL BODY").unwrap();
        assert!(out.claude_mcp_config_path.is_none());
        assert_eq!(out.codex_extra_args.len(), 4);
        assert_eq!(out.codex_extra_args[0], "-c");
        assert!(out.codex_extra_args[1].contains("mcp_servers.coffee-cli.url"));
        assert!(out.codex_extra_args[1].contains("http://127.0.0.1:50000/mcp"));
        assert_eq!(out.codex_extra_args[2], "-c");
        assert!(out.codex_extra_args[3].contains("model_instructions_file"));
        // Protocol text actually got written.
        let inst_path = panes_root()
            .join(sanitize_pane_id(&pid))
            .join("instructions.md");
        let body = fs::read_to_string(&inst_path).unwrap();
        assert_eq!(body, "PROTOCOL BODY");
        let _ = fs::remove_dir_all(panes_root().join(sanitize_pane_id(&pid)));
    }

    #[test]
    fn unknown_cli_kind_is_a_noop() {
        let pid = unique_pane("unknown");
        let out = prepare_pane_config_dir(&pid, "unsupported", &ep(), "ignored").unwrap();
        assert!(out.claude_mcp_config_path.is_none());
        assert!(out.codex_extra_args.is_empty());
        let _ = fs::remove_dir_all(panes_root().join(sanitize_pane_id(&pid)));
    }
}
