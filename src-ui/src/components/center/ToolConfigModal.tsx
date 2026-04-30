// Per-tool launch override modal.
//
// Reached from the small gear icon that appears on launchpad cards when
// hovered. Lets users customize how a specific CLI tool gets spawned
// — for cases where the built-in `where claude` / `which claude`
// auto-detect can't find the binary (WSL Hermes, conda envs, custom
// forks, docker exec, etc).
//
// All 4 fields are optional — empty falls through to the built-in
// default. Persisted via the backend Tauri command into
// `~/.coffee-cli/tools.json` (atomic write).

import { useEffect, useState } from 'react';
import { commands, type ToolConfigEntry } from '../../tauri';

interface Props {
  toolKey: string;
  toolLabel: string;
  onClose: () => void;
}

const EMPTY: ToolConfigEntry = {
  command: '',
  extra_args: [],
  default_cwd: '',
  history_path: '',
};

export function ToolConfigModal({ toolKey, toolLabel, onClose }: Props) {
  const [entry, setEntry] = useState<ToolConfigEntry>(EMPTY);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [extraArgsText, setExtraArgsText] = useState('');

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const e = await commands.getToolConfig(toolKey);
        if (cancelled) return;
        setEntry(e);
        setExtraArgsText(e.extra_args.join('\n'));
      } catch (err) {
        // eslint-disable-next-line no-console
        console.warn('[tool-config] load failed:', err);
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => { cancelled = true; };
  }, [toolKey]);

  const handleSave = async () => {
    setSaving(true);
    try {
      const args = extraArgsText
        .split('\n')
        .map(s => s.trim())
        .filter(Boolean);
      await commands.setToolConfig(toolKey, {
        command: entry.command.trim(),
        extra_args: args,
        default_cwd: entry.default_cwd.trim(),
        history_path: entry.history_path.trim(),
      });
      onClose();
    } catch (err) {
      // eslint-disable-next-line no-console
      console.error('[tool-config] save failed:', err);
      alert('Save failed: ' + String(err));
    } finally {
      setSaving(false);
    }
  };

  const handleReset = async () => {
    if (!confirm('Reset all custom settings for ' + toolLabel + '?')) return;
    setSaving(true);
    try {
      await commands.setToolConfig(toolKey, EMPTY);
      setEntry(EMPTY);
      setExtraArgsText('');
      onClose();
    } finally {
      setSaving(false);
    }
  };

  return (
    <div
      onClick={onClose}
      style={{
        position: 'fixed',
        inset: 0,
        background: 'rgba(0,0,0,0.55)',
        backdropFilter: 'blur(4px)',
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        zIndex: 1000,
      }}
    >
      <div
        onClick={(e) => e.stopPropagation()}
        style={{
          width: 'min(560px, 92vw)',
          maxHeight: '88vh',
          overflowY: 'auto',
          background: 'var(--bg-color, #15151a)',
          border: '1px solid var(--border, rgba(255,255,255,0.12))',
          borderRadius: 10,
          padding: '24px 26px',
          color: 'var(--text-primary)',
          fontFamily: 'var(--font-mono, ui-monospace, Menlo, Consolas, monospace)',
          fontSize: 13,
          boxShadow: '0 24px 60px -16px rgba(0,0,0,0.5)',
        }}
      >
        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: 4 }}>
          <h2 style={{ margin: 0, fontSize: 16, fontWeight: 600 }}>
            {toolLabel} <span style={{ opacity: 0.5, fontWeight: 400 }}>· launch settings</span>
          </h2>
          <button
            onClick={onClose}
            style={{
              background: 'transparent',
              border: 0,
              color: 'inherit',
              cursor: 'pointer',
              fontSize: 18,
              opacity: 0.6,
              padding: '0 4px',
            }}
          >×</button>
        </div>
        <p style={{ marginTop: 6, marginBottom: 18, opacity: 0.6, fontSize: 12, lineHeight: 1.55 }}>
          All fields are optional. Empty = use Coffee CLI's built-in default.
          For WSL: e.g. command <code style={{ opacity: 0.8 }}>wsl ~/.local/bin/hermes</code>.
        </p>

        {loading ? (
          <p style={{ opacity: 0.5 }}>Loading…</p>
        ) : (
          <>
            <Field
              label="Launch command"
              hint="e.g. wsl ~/.local/bin/hermes — first token is the binary, rest are prepended to args. Empty = use PATH."
              value={entry.command}
              onChange={v => setEntry({ ...entry, command: v })}
              placeholder={defaultCommandFor(toolKey)}
            />

            <FieldMultiline
              label="Extra launch args"
              hint="One per line. Appended after the built-in args. Example: --dangerously-skip-permissions"
              value={extraArgsText}
              onChange={setExtraArgsText}
              rows={3}
            />

            <Field
              label="Default working directory"
              hint="Pre-fills the folder selector when starting a new tab. Empty = use the launchpad's last-used cwd."
              value={entry.default_cwd}
              onChange={v => setEntry({ ...entry, default_cwd: v })}
              placeholder="(empty — fall back to last-used)"
            />

            <Field
              label="Session history path"
              hint="Directory containing this tool's session files. Useful for WSL — e.g. \\\\wsl.localhost\\Ubuntu\\home\\user\\.hermes\\sessions"
              value={entry.history_path}
              onChange={v => setEntry({ ...entry, history_path: v })}
              placeholder={defaultHistoryFor(toolKey)}
            />
          </>
        )}

        <div style={{ display: 'flex', gap: 8, justifyContent: 'flex-end', marginTop: 22 }}>
          <button
            onClick={handleReset}
            disabled={saving || loading}
            style={btnStyle('subtle')}
          >
            Reset
          </button>
          <button
            onClick={onClose}
            disabled={saving}
            style={btnStyle('subtle')}
          >
            Cancel
          </button>
          <button
            onClick={handleSave}
            disabled={saving || loading}
            style={btnStyle('primary')}
          >
            {saving ? 'Saving…' : 'Save'}
          </button>
        </div>
      </div>
    </div>
  );
}

function Field({
  label, hint, value, onChange, placeholder,
}: {
  label: string;
  hint: string;
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
}) {
  return (
    <div style={{ marginBottom: 16 }}>
      <label style={{ display: 'block', fontSize: 12, fontWeight: 600, marginBottom: 4 }}>{label}</label>
      <input
        type="text"
        value={value}
        onChange={e => onChange(e.target.value)}
        placeholder={placeholder}
        spellCheck={false}
        style={{
          width: '100%',
          padding: '7px 10px',
          background: 'rgba(255,255,255,0.04)',
          border: '1px solid var(--border, rgba(255,255,255,0.12))',
          borderRadius: 5,
          color: 'inherit',
          fontFamily: 'inherit',
          fontSize: 12.5,
          outline: 'none',
        }}
      />
      <p style={{ marginTop: 4, marginBottom: 0, fontSize: 11, opacity: 0.5, lineHeight: 1.5 }}>{hint}</p>
    </div>
  );
}

function FieldMultiline({
  label, hint, value, onChange, rows,
}: {
  label: string;
  hint: string;
  value: string;
  onChange: (v: string) => void;
  rows: number;
}) {
  return (
    <div style={{ marginBottom: 16 }}>
      <label style={{ display: 'block', fontSize: 12, fontWeight: 600, marginBottom: 4 }}>{label}</label>
      <textarea
        value={value}
        onChange={e => onChange(e.target.value)}
        rows={rows}
        spellCheck={false}
        style={{
          width: '100%',
          padding: '7px 10px',
          background: 'rgba(255,255,255,0.04)',
          border: '1px solid var(--border, rgba(255,255,255,0.12))',
          borderRadius: 5,
          color: 'inherit',
          fontFamily: 'inherit',
          fontSize: 12.5,
          outline: 'none',
          resize: 'vertical',
        }}
      />
      <p style={{ marginTop: 4, marginBottom: 0, fontSize: 11, opacity: 0.5, lineHeight: 1.5 }}>{hint}</p>
    </div>
  );
}

function btnStyle(kind: 'primary' | 'subtle'): React.CSSProperties {
  const base: React.CSSProperties = {
    padding: '7px 16px',
    fontSize: 13,
    borderRadius: 5,
    cursor: 'pointer',
    fontFamily: 'inherit',
    border: '1px solid var(--border, rgba(255,255,255,0.15))',
  };
  if (kind === 'primary') {
    return {
      ...base,
      background: 'var(--accent, #c4956a)',
      color: '#1a1a1c',
      fontWeight: 600,
      borderColor: 'transparent',
    };
  }
  return {
    ...base,
    background: 'transparent',
    color: 'inherit',
  };
}

function defaultCommandFor(tool: string): string {
  switch (tool) {
    case 'claude':   return 'claude';
    case 'codex':    return 'codex';
    case 'gemini':   return 'gemini';
    case 'qwen':     return 'qwen';
    case 'opencode': return 'opencode';
    case 'openclaw': return 'openclaw';
    case 'hermes':   return 'hermes';
    default:         return '';
  }
}

function defaultHistoryFor(tool: string): string {
  switch (tool) {
    case 'claude':   return '~/.claude/projects';
    case 'codex':    return '~/.codex/sessions';
    case 'gemini':   return '~/.gemini/tmp';
    case 'hermes':   return '~/.hermes/sessions';
    case 'opencode': return '~/.local/share/opencode';
    default:         return '';
  }
}
