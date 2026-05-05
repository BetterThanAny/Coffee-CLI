# Review and Fix Report

## Changes
- Added a guard in `fs_paste` to reject copying a directory into itself or one of its descendants, preventing recursive growth.
- Removed editable `command` and `extra_args` controls from `ToolConfigModal`; the UI now matches the backend policy that refuses command overrides.
- Added Rust tests for rejected descendant copies and normal sibling directory copies.

## Verification
- `cargo test fs_paste` passed.
- `cargo test tool_config` passed.
- `cd src-ui && npm run build` passed.
- `git diff --check` passed.

## Remaining
- The MCP token-in-URL design was not changed. That touches the Codex/Claude MCP injection contract and needs a separate compatibility pass.
