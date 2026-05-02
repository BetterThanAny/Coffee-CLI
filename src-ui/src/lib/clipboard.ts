// clipboard.ts — unified clipboard I/O for the entire UI.
//
// All clipboard reads/writes MUST go through these helpers. Writes use
// Tauri's plugin for reliable desktop UX; reads intentionally use the browser
// Clipboard API so the WebView permission model still applies.
//
// If you need a new context menu or keyboard shortcut that touches
// the clipboard, import from here. Do not re-derive.

import { writeText } from '@tauri-apps/plugin-clipboard-manager';

/** Write text to the system clipboard. Silently swallows failures
 *  because clipboard writes are always best-effort UX glue — we never
 *  want a rejected promise to break the caller. */
export function clipboardWrite(text: string): Promise<void> {
  return writeText(text).catch(() => {});
}

/** Read text from the system clipboard. Returns empty string on any
 *  failure (empty clipboard, permission denied, etc.) so callers can
 *  do a simple `if (text)` check without try/catch boilerplate. */
export function clipboardRead(): Promise<string> {
  if (typeof navigator === 'undefined' || !navigator.clipboard?.readText) {
    return Promise.resolve('');
  }
  return navigator.clipboard.readText().then(t => t ?? '').catch(() => '');
}
