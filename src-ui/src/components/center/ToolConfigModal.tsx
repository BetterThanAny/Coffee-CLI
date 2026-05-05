// Per-tool configuration modal — minimal version.
//
// Reached from the small gear on Library cards. Lets the user override
// safe per-tool paths: default cwd and custom history scan path. Empty
// fields fall through to Coffee CLI's built-in default.
//
// Stripped of explanatory text on purpose: anyone reaching this modal
// is already comfortable with CLI args + paths. 2 field labels + 3
// button labels = 5 total i18n keys, nothing else.
//
// Styling references theme tokens (--bg-panel / --text-1 / --accent
// etc.) so dark and light themes are picked up automatically — no
// hard-coded rgba values that go invisible on white.
//
// Persisted via Tauri command into ~/.coffee-cli/tools.json.

import { useEffect, useMemo, useState } from 'react';
import { commands, type ToolConfigEntry } from '../../tauri';
import { useT } from '../../i18n/useT';

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

// Built-in safe-path defaults. Used as the initial form values when no user
// override exists, and as the target of the Reset button. Launch command and
// extra args intentionally stay empty because the backend rejects them.
const TOOL_DEFAULTS: Record<string, ToolConfigEntry> = {
  claude:   { command: '', extra_args: [], default_cwd: '', history_path: '~/.claude/projects' },
  codex:    { command: '', extra_args: [], default_cwd: '', history_path: '~/.codex/sessions' },
};

// Tools whose session history Coffee CLI's history scanner actually reads
// (load_native_history_blocking in src/server.rs). For these we surface
// the history_path field.
const HISTORY_SCANNED_TOOLS = new Set(['claude', 'codex']);

const defaultsFor = (key: string): ToolConfigEntry => TOOL_DEFAULTS[key] ?? EMPTY;

function withFallback(user: ToolConfigEntry, def: ToolConfigEntry): ToolConfigEntry {
  return {
    command:      '',
    extra_args:   [],
    default_cwd:  user.default_cwd  || def.default_cwd,
    history_path: user.history_path || def.history_path,
  };
}

function diffField<T extends string | string[]>(value: T, defaultValue: T): T {
  if (Array.isArray(value) && Array.isArray(defaultValue)) {
    const a = value.join('\n'); const b = defaultValue.join('\n');
    return (a === b ? ([] as unknown as T) : value);
  }
  return value === defaultValue ? ('' as unknown as T) : value;
}

export function ToolConfigModal({ toolKey, toolLabel, onClose }: Props) {
  const t = useT();
  const def = useMemo(() => defaultsFor(toolKey), [toolKey]);
  const [entry, setEntry] = useState<ToolConfigEntry>(def);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const user = await commands.getToolConfig(toolKey);
        if (cancelled) return;
        const merged = withFallback(user, def);
        setEntry(merged);
      } catch {
        /* swallow — leave the form at defaults */
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => { cancelled = true; };
  }, [toolKey, def]);

  const handleSave = async () => {
    setSaving(true);
    try {
      const payload: ToolConfigEntry = {
        command:      '',
        extra_args:   [],
        default_cwd:  diffField(entry.default_cwd.trim(), def.default_cwd),
        history_path: diffField(entry.history_path.trim(), def.history_path),
      };
      await commands.setToolConfig(toolKey, payload);
      onClose();
    } finally {
      setSaving(false);
    }
  };

  const handleReset = async () => {
    setSaving(true);
    try {
      await commands.setToolConfig(toolKey, EMPTY);
      setEntry(def);
    } finally {
      setSaving(false);
    }
  };

  return (
    <div onClick={onClose} className="tool-config-backdrop">
      <div onClick={e => e.stopPropagation()} className="tool-config-modal">
        <div className="tool-config-header">
          <span>{toolLabel}</span>
          <button onClick={onClose} className="tool-config-close" aria-label="close">×</button>
        </div>

        {!loading && (
          <>
            <Field
              label={t('tool_config.default_cwd' as any)}
              value={entry.default_cwd}
              onChange={v => setEntry({ ...entry, default_cwd: v })}
            />
            {HISTORY_SCANNED_TOOLS.has(toolKey) && (
              <Field
                label={t('tool_config.history_path' as any)}
                value={entry.history_path}
                onChange={v => setEntry({ ...entry, history_path: v })}
              />
            )}
          </>
        )}

        <div className="tool-config-buttons">
          <button onClick={handleReset} disabled={saving || loading} className="tool-config-btn tool-config-btn-subtle">
            {t('tool_config.reset' as any)}
          </button>
          <button onClick={onClose} disabled={saving} className="tool-config-btn tool-config-btn-subtle">
            {t('tool_config.cancel' as any)}
          </button>
          <button onClick={handleSave} disabled={saving || loading} className="tool-config-btn tool-config-btn-primary">
            {t('tool_config.save' as any)}
          </button>
        </div>
      </div>
    </div>
  );
}

function Field({
  label, value, onChange, multiline, rows,
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  multiline?: boolean;
  rows?: number;
}) {
  return (
    <label className="tool-config-field">
      <span className="tool-config-field-label">{label}</span>
      {multiline ? (
        <textarea
          value={value}
          onChange={e => onChange(e.target.value)}
          rows={rows ?? 3}
          spellCheck={false}
          className="tool-config-input"
        />
      ) : (
        <input
          type="text"
          value={value}
          onChange={e => onChange(e.target.value)}
          spellCheck={false}
          className="tool-config-input"
        />
      )}
    </label>
  );
}
