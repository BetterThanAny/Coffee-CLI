use crate::terminal;

use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use tauri::{Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;

/// Shared app state
pub struct AppState {
    pub terminal_session: terminal::SharedSession,
    /// Loopback port of the hook TCP server (set once during setup).
    /// 0 means the hook server failed to start; env var injection is skipped in that case.
    pub hook_port: std::sync::atomic::AtomicU16,
    /// Per-process bearer token required by the hook TCP bridge.
    /// Injected only into spawned Claude panes; never exposed to the UI.
    pub hook_token: String,
    /// Active OS fs watcher (one per app instance). Some(...) while a
    /// workspace folder is open; None otherwise. Swapping this Mutex'd
    /// Option replaces the watcher atomically on folder switch.
    pub fs_watcher: Mutex<Option<crate::fs_watcher::FsWatcher>>,
    /// Per-pane MCP server endpoints. Each multi-agent pane (Claude /
    /// Codex) gets its OWN HTTP listener on its own port with
    /// `self_pane_id` baked in, so `whoami()` / `is_self` in
    /// `list_panes` / `[From <id>]` prefixing in `send_to_pane` all
    /// behave deterministically regardless of which CLI is calling.
    /// Map is keyed by pane id (= terminal session id like
    /// "tab-X::pane-2"). Endpoints persist for the app lifetime —
    /// TCP listeners are cheap and bounded by max concurrent panes.
    pub pane_mcp_endpoints:
        tokio::sync::Mutex<std::collections::HashMap<String, crate::mcp_server::McpEndpoint>>,
    /// Async lock around any MCP-server spawn path. Held only while
    /// a (one-time) spawn is in flight so concurrent first-callers
    /// don't race and bind two listeners for the same pane.
    pub mcp_spawn_lock: tokio::sync::Mutex<()>,
}

#[tauri::command]
fn window_minimize(window: tauri::Window) {
    let _ = window.minimize();
}

#[tauri::command]
fn window_maximize(window: tauri::Window) {
    let is_max = window.is_maximized().unwrap_or(false);
    if is_max {
        let _ = window.unmaximize();
    } else {
        let _ = window.maximize();
    }
}

#[tauri::command]
fn window_close(window: tauri::Window, app: tauri::AppHandle) {
    let label = window.label().to_string();
    if label == "main" {
        // Main window: close entire application (including all detached windows)
        app.exit(0);
    } else {
        // Detached window: just close this one
        let _ = window.close();
    }
}

#[tauri::command]
fn show_main_window(app: tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

#[tauri::command]
async fn pick_folder(app: tauri::AppHandle) -> Result<String, String> {
    let folder = app.dialog().file().blocking_pick_folder();

    match folder {
        Some(path) => Ok(path.to_string()),
        None => Err("cancelled".to_string()),
    }
}

// ─── Tool Availability Detection ─────────────────────────────────────────────

#[cfg(target_os = "windows")]
fn check_tool_windows(bin: &str) -> bool {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("where")
        .arg(bin)
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(target_os = "windows"))]
fn check_tool_unix(bin: &str) -> bool {
    std::process::Command::new("which")
        .arg(bin)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[tauri::command]
fn check_tools_installed() -> std::collections::HashMap<String, bool> {
    let tools = vec![
        ("claude", "claude"),
        ("codex", "codex"),
        // remote is always available — it's just SSH (built into the OS)
    ];
    let mut result = std::collections::HashMap::new();
    for (key, bin) in tools {
        #[cfg(target_os = "windows")]
        let found = check_tool_windows(bin);

        #[cfg(not(target_os = "windows"))]
        let found = check_tool_unix(bin);

        result.insert(key.to_string(), found);
    }
    // Terminal is always available — it's the system shell
    result.insert("terminal".to_string(), true);
    result
}

// ─── File System Live Watcher ────────────────────────────────────────────────
//
// Start/stop a recursive fs watcher on the workspace folder so changes
// made by external tools (terminal CLIs, editors, git, etc.) propagate
// into the Explorer tree immediately. See fs_watcher.rs for mechanics.

#[tauri::command]
fn start_fs_watcher(
    path: String,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let root = validate_fs_path(&path, false)?;
    let watcher = crate::fs_watcher::FsWatcher::start(app, root)?;
    // Replace atomically; dropping the old FsWatcher stops its OS handle.
    let mut guard = state
        .fs_watcher
        .lock()
        .map_err(|e| format!("lock: {}", e))?;
    *guard = Some(watcher);
    Ok(())
}

#[tauri::command]
fn stop_fs_watcher(state: State<'_, AppState>) -> Result<(), String> {
    let mut guard = state
        .fs_watcher
        .lock()
        .map_err(|e| format!("lock: {}", e))?;
    // Drop releases the OS watcher handle.
    *guard = None;
    Ok(())
}

// ─── File System Browsing API ────────────────────────────────────────────────

/// Information about a single drive / mount point
#[derive(Serialize)]
struct DriveInfo {
    path: String,
    label: String,
    /// Semantic kind used by frontend for icon selection and i18n.
    /// Values: "desktop", "downloads", "documents", "pictures", "music", "videos", "home", "drive", "root", "volume"
    kind: String,
}

/// Information about a single directory entry (file or folder)
#[derive(Serialize)]
struct DirEntry {
    name: String,
    path: String,
    is_dir: bool,
    size: u64,
}

/// List all available drives (Windows) or common mount points (Unix)
#[tauri::command]
fn list_drives() -> Vec<DriveInfo> {
    let mut drives: Vec<DriveInfo> = Vec::new();

    // ── Quick Access locations (order matches Windows Explorer) ──

    if let Some(desktop) = dirs::desktop_dir() {
        if desktop.exists() {
            drives.push(DriveInfo {
                path: desktop.to_string_lossy().to_string(),
                label: "Desktop".to_string(),
                kind: "desktop".to_string(),
            });
        }
    }

    if let Some(dl) = dirs::download_dir() {
        if dl.exists() {
            drives.push(DriveInfo {
                path: dl.to_string_lossy().to_string(),
                label: "Downloads".to_string(),
                kind: "downloads".to_string(),
            });
        }
    }

    if let Some(docs) = dirs::document_dir() {
        if docs.exists() {
            drives.push(DriveInfo {
                path: docs.to_string_lossy().to_string(),
                label: "Documents".to_string(),
                kind: "documents".to_string(),
            });
        }
    }

    if let Some(pics) = dirs::picture_dir() {
        if pics.exists() {
            drives.push(DriveInfo {
                path: pics.to_string_lossy().to_string(),
                label: "Pictures".to_string(),
                kind: "pictures".to_string(),
            });
        }
    }

    if let Some(music) = dirs::audio_dir() {
        if music.exists() {
            drives.push(DriveInfo {
                path: music.to_string_lossy().to_string(),
                label: "Music".to_string(),
                kind: "music".to_string(),
            });
        }
    }

    if let Some(videos) = dirs::video_dir() {
        if videos.exists() {
            drives.push(DriveInfo {
                path: videos.to_string_lossy().to_string(),
                label: "Videos".to_string(),
                kind: "videos".to_string(),
            });
        }
    }

    // Home directory
    if let Some(home) = dirs::home_dir() {
        drives.push(DriveInfo {
            path: home.to_string_lossy().to_string(),
            label: "Home".to_string(),
            kind: "home".to_string(),
        });
    }

    // ── Disk Drives ──

    #[cfg(target_os = "windows")]
    {
        for letter in b'A'..=b'Z' {
            let drive_path = format!("{}:\\", letter as char);
            let p = std::path::Path::new(&drive_path);
            if p.exists() {
                drives.push(DriveInfo {
                    path: drive_path.clone(),
                    label: format!("{}", letter as char),
                    kind: "drive".to_string(),
                });
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        if cfg!(target_os = "macos") {
            if let Ok(entries) = std::fs::read_dir("/Volumes") {
                for entry in entries.flatten() {
                    if entry.path().is_dir() {
                        drives.push(DriveInfo {
                            path: entry.path().to_string_lossy().to_string(),
                            label: entry.file_name().to_string_lossy().to_string(),
                            kind: "volume".to_string(),
                        });
                    }
                }
            }
        }
    }

    drives
}

/// List the immediate children of a directory.
/// Returns files and subdirectories sorted: directories first, then files, both alphabetical.
#[tauri::command]
fn list_directory(path: String) -> Result<Vec<DirEntry>, String> {
    let dir = validate_fs_path(&path, false)?;
    if !dir.is_dir() {
        return Err(format!("Not a directory: {}", path));
    }

    let mut entries: Vec<DirEntry> = Vec::new();

    let read_dir = std::fs::read_dir(&dir).map_err(|e| format!("Cannot read directory: {}", e))?;

    for entry in read_dir {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue, // Skip unreadable entries
        };
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || has_protected_component(&entry.path()) {
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue, // Skip unreadable entries
        };

        entries.push(DirEntry {
            name,
            path: entry.path().to_string_lossy().to_string(),
            is_dir: metadata.is_dir(),
            size: metadata.len(),
        });
    }

    // Sort: directories first, then files, both alphabetical (case insensitive)
    entries.sort_by(|a, b| {
        if a.is_dir != b.is_dir {
            return if a.is_dir {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
        }
        a.name.to_lowercase().cmp(&b.name.to_lowercase())
    });

    Ok(entries)
}

/// Check whether a Claude Code skill is installed locally.
/// Returns true if `~/.claude/skills/<name>/SKILL.md` exists.
#[tauri::command]
fn check_skill_installed(name: String) -> bool {
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    home.join(".claude")
        .join("skills")
        .join(&name)
        .join("SKILL.md")
        .exists()
}

/// Check whether the Claude Code `/insights` usage report exists.
/// Returns true if `~/.claude/usage-data/report.html` is present.
/// Used by the VibeID launcher to decide whether to first auto-run
/// `/insights` in a pre-run tab, or go straight to `/vibeid`.
#[tauri::command]
fn check_vibeid_report_exists() -> bool {
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    home.join(".claude")
        .join("usage-data")
        .join("report.html")
        .exists()
}

/// Return the Unix epoch seconds of the last modification time of the
/// `~/.claude/usage-data/report.html` file. Returns 0 if the file does
/// not exist or if metadata cannot be read.
///
/// Used by the VibeID launcher to detect when a pre-run `/insights`
/// invocation has finished writing a fresh report (mtime strictly
/// greater than the click timestamp means the report was regenerated).
#[tauri::command]
fn check_vibeid_report_mtime() -> u64 {
    let Some(home) = dirs::home_dir() else {
        return 0;
    };
    let path = home.join(".claude").join("usage-data").join("report.html");
    match std::fs::metadata(&path).and_then(|m| m.modified()) {
        Ok(mtime) => mtime
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        Err(_) => 0,
    }
}

#[tauri::command]
fn install_vibeid_skill(lang: String) -> Result<(), String> {
    let home = dirs::home_dir().ok_or_else(|| "No home directory".to_string())?;
    let root = home.join(".claude").join("skills").join("vibeid");
    let lang = lang.trim();
    if lang.len() > 32
        || lang.contains('\0')
        || lang.contains('/')
        || lang.contains('\\')
        || lang.contains(':')
        || lang.contains("..")
    {
        return Err("Invalid language tag".to_string());
    }

    const FILES: &[(&str, &[u8])] = &[
        (
            "SKILL.md",
            include_bytes!("../Web-Home/CC-VibeID-test/SKILL.md"),
        ),
        (
            "matrix.json",
            include_bytes!("../Web-Home/CC-VibeID-test/matrix.json"),
        ),
        (
            "scripts/analyze.js",
            include_bytes!("../Web-Home/CC-VibeID-test/scripts/analyze.js"),
        ),
        (
            "scripts/inject.js",
            include_bytes!("../Web-Home/CC-VibeID-test/scripts/inject.js"),
        ),
        (
            "images/EDAH.png",
            include_bytes!("../Web-Home/CC-VibeID-test/personas/images/EDAH.png"),
        ),
        (
            "images/EDAL.png",
            include_bytes!("../Web-Home/CC-VibeID-test/personas/images/EDAL.png"),
        ),
        (
            "images/EDVH.png",
            include_bytes!("../Web-Home/CC-VibeID-test/personas/images/EDVH.png"),
        ),
        (
            "images/EDVL.png",
            include_bytes!("../Web-Home/CC-VibeID-test/personas/images/EDVL.png"),
        ),
        (
            "images/ETAH.png",
            include_bytes!("../Web-Home/CC-VibeID-test/personas/images/ETAH.png"),
        ),
        (
            "images/ETAL.png",
            include_bytes!("../Web-Home/CC-VibeID-test/personas/images/ETAL.png"),
        ),
        (
            "images/ETVH.png",
            include_bytes!("../Web-Home/CC-VibeID-test/personas/images/ETVH.png"),
        ),
        (
            "images/ETVL.png",
            include_bytes!("../Web-Home/CC-VibeID-test/personas/images/ETVL.png"),
        ),
        (
            "images/RDAH.png",
            include_bytes!("../Web-Home/CC-VibeID-test/personas/images/RDAH.png"),
        ),
        (
            "images/RDAL.png",
            include_bytes!("../Web-Home/CC-VibeID-test/personas/images/RDAL.png"),
        ),
        (
            "images/RDVH.png",
            include_bytes!("../Web-Home/CC-VibeID-test/personas/images/RDVH.png"),
        ),
        (
            "images/RDVL.png",
            include_bytes!("../Web-Home/CC-VibeID-test/personas/images/RDVL.png"),
        ),
        (
            "images/RTAH.png",
            include_bytes!("../Web-Home/CC-VibeID-test/personas/images/RTAH.png"),
        ),
        (
            "images/RTAL.png",
            include_bytes!("../Web-Home/CC-VibeID-test/personas/images/RTAL.png"),
        ),
        (
            "images/RTVH.png",
            include_bytes!("../Web-Home/CC-VibeID-test/personas/images/RTVH.png"),
        ),
        (
            "images/RTVL.png",
            include_bytes!("../Web-Home/CC-VibeID-test/personas/images/RTVL.png"),
        ),
    ];

    for (rel, bytes) in FILES {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create skill dir: {e}"))?;
        }
        std::fs::write(&path, bytes)
            .map_err(|e| format!("Failed to write {}: {e}", path.display()))?;
    }

    std::fs::write(root.join(".user_lang"), lang.as_bytes())
        .map_err(|e| format!("Failed to write language hint: {e}"))?;
    Ok(())
}

/// Save a base64-encoded clipboard image to a temp file.
/// Used by the Gambit compose window so pasted screenshots can be referenced
/// by path when forwarded to AI CLI agents (Claude Code, etc.).
///
/// Guards:
/// - Extension whitelisted to common raster formats
/// - Hard 25 MB size cap to prevent runaway base64 payloads filling the disk
/// - Filename uses pid + atomic counter so two concurrent paste calls (same
///   millisecond) can never collide and truncate each other's file
#[tauri::command]
fn save_clipboard_image(data_base64: String, extension: String) -> Result<String, String> {
    use base64::{engine::general_purpose, Engine as _};
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    const MAX_BYTES: usize = 25 * 1024 * 1024; // 25 MB

    // Only allow common web image formats. Block anything that could
    // execute or exploit a path-traversal quirk in the extension.
    let ext = match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => extension,
        _ => return Err(format!("Unsupported image extension: {}", extension)),
    };

    let bytes = general_purpose::STANDARD
        .decode(&data_base64)
        .map_err(|e| format!("base64 decode: {}", e))?;

    if bytes.len() > MAX_BYTES {
        return Err(format!(
            "Image too large: {} bytes (max {})",
            bytes.len(),
            MAX_BYTES
        ));
    }

    let tmp_dir = std::env::temp_dir()
        .join("coffee-cli")
        .join("pasted-images");
    std::fs::create_dir_all(&tmp_dir).map_err(|e| format!("mkdir: {}", e))?;

    static SEQ: AtomicU64 = AtomicU64::new(0);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let pid = std::process::id();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = tmp_dir.join(format!("clip-{}-{}-{}.{}", stamp, pid, seq, ext));

    let mut file = std::fs::File::create(&path).map_err(|e| format!("create image file: {}", e))?;
    file.write_all(&bytes)
        .map_err(|e| format!("write image bytes: {}", e))?;

    Ok(path.to_string_lossy().to_string())
}

// ─── File System Operations ───────────────────────────────────────────────────

/// Open the native file explorer and highlight / reveal the given path.
#[tauri::command]
fn show_in_folder(path: String) -> Result<(), String> {
    let safe_path = validate_fs_path(&path, false)?;
    #[cfg(target_os = "windows")]
    {
        // explorer /select, highlights the item in its parent folder.
        // The frontend normalizes paths to forward slashes, but explorer.exe
        // requires backslashes — forward slashes cause it to silently open Desktop.
        let win_path = safe_path.to_string_lossy().replace('/', "\\");
        std::process::Command::new("explorer")
            .arg("/select,")
            .arg(&win_path)
            .spawn()
            .map_err(|e| format!("Failed to open Explorer: {e}"))?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("-R") // Reveal in Finder
            .arg(&safe_path)
            .spawn()
            .map_err(|e| format!("Failed to open Finder: {e}"))?;
    }
    #[cfg(target_os = "linux")]
    {
        // Open the parent directory; most Linux file managers don't support select
        let dir = if safe_path.is_dir() {
            safe_path.clone()
        } else {
            safe_path.parent().unwrap_or(&safe_path).to_path_buf()
        };
        std::process::Command::new("xdg-open")
            .arg(dir)
            .spawn()
            .map_err(|e| format!("Failed to open file manager: {e}"))?;
    }
    Ok(())
}

fn normalize_canonical(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if s.starts_with(r"\\?\") {
            return PathBuf::from(s[4..].to_string());
        }
    }
    path
}

fn canonical_existing(path: &Path) -> Result<PathBuf, String> {
    path.canonicalize()
        .map(normalize_canonical)
        .map_err(|e| format!("Invalid path: {e}"))
}

fn is_under_dir(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

fn is_allowed_user_root(path: &Path) -> bool {
    if let Some(home) = dirs::home_dir()
        .and_then(|h| h.canonicalize().ok())
        .map(normalize_canonical)
    {
        if is_under_dir(path, &home) {
            return true;
        }
    }

    #[cfg(target_os = "macos")]
    {
        let volumes = Path::new("/Volumes");
        if path.starts_with(volumes) && path.components().count() >= 3 {
            return true;
        }
    }

    #[cfg(target_os = "linux")]
    {
        for root in [Path::new("/mnt"), Path::new("/media")] {
            if path.starts_with(root) && path.components().count() >= 3 {
                return true;
            }
        }
    }

    false
}

fn has_protected_component(path: &Path) -> bool {
    const PROTECTED: &[&str] = &[
        ".ssh",
        ".gnupg",
        ".aws",
        ".azure",
        ".config",
        ".kube",
        ".docker",
        ".1password",
        ".claude",
        ".codex",
        ".gemini",
        ".coffee-cli",
    ];
    path.components().any(|component| match component {
        Component::Normal(name) => name
            .to_str()
            .map(|s| PROTECTED.contains(&s))
            .unwrap_or(false),
        _ => false,
    })
}

/// Validate a user-facing filesystem path.
///
/// The Explorer intentionally browses user files, but it should not become a
/// generic `/etc` / `C:\Windows` enumerator or a mutation API for credential
/// directories if the WebView is compromised.
fn validate_fs_path(path: &str, allow_protected: bool) -> Result<PathBuf, String> {
    let canonical = canonical_existing(Path::new(path))?;
    if canonical.components().count() < 3 {
        return Err("Operation rejected: path is too shallow".to_string());
    }
    if !is_allowed_user_root(&canonical) {
        return Err("Operation rejected: path is outside user file areas".to_string());
    }
    if !allow_protected && has_protected_component(&canonical) {
        return Err("Operation rejected: protected configuration directory".to_string());
    }
    Ok(canonical)
}

fn validate_new_leaf_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name == "." || name == ".." {
        return Err("Invalid name".to_string());
    }
    if name.contains('\0')
        || name.contains('/')
        || name.contains('\\')
        || name.contains(':')
        || Path::new(name).is_absolute()
    {
        return Err("Name must be a single filename, not a path".to_string());
    }
    Ok(())
}

fn move_to_trash(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        fn apple_string(input: &str) -> String {
            input.replace('\\', "\\\\").replace('"', "\\\"")
        }
        let script = format!(
            "tell application \"Finder\" to delete POSIX file \"{}\"",
            apple_string(&path.to_string_lossy())
        );
        let status = std::process::Command::new("osascript")
            .arg("-e")
            .arg(script)
            .status()
            .map_err(|e| format!("Move to trash failed: {e}"))?;
        if status.success() {
            return Ok(());
        }
        return Err("Move to trash failed".to_string());
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let method = if path.is_dir() {
            "DeleteDirectory"
        } else {
            "DeleteFile"
        };
        let script = format!(
            "Add-Type -AssemblyName Microsoft.VisualBasic; \
             [Microsoft.VisualBasic.FileIO.FileSystem]::{method}($args[0], \
             'OnlyErrorDialogs', 'SendToRecycleBin')"
        );
        let status = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .arg(path)
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .map_err(|e| format!("Move to recycle bin failed: {e}"))?;
        if status.success() {
            return Ok(());
        }
        return Err("Move to recycle bin failed".to_string());
    }

    #[cfg(target_os = "linux")]
    {
        let status = std::process::Command::new("gio")
            .arg("trash")
            .arg(path)
            .status();
        if matches!(status, Ok(s) if s.success()) {
            return Ok(());
        }

        let home = dirs::home_dir().ok_or_else(|| "No home directory".to_string())?;
        let trash_dir = home
            .join(".local")
            .join("share")
            .join("Trash")
            .join("files");
        std::fs::create_dir_all(&trash_dir)
            .map_err(|e| format!("Create trash directory failed: {e}"))?;
        let file_name = path.file_name().ok_or_else(|| "Invalid path".to_string())?;
        let mut dest = trash_dir.join(file_name);
        let mut i = 0u32;
        while dest.exists() {
            i += 1;
            let suffix = format!(".{}", i);
            dest = trash_dir.join(format!("{}{}", file_name.to_string_lossy(), suffix));
        }
        std::fs::rename(path, dest).map_err(|e| format!("Move to trash failed: {e}"))
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = path;
        Err("Move to trash is unsupported on this platform".to_string())
    }
}

/// Delete a file or directory via the platform trash/recycle bin.
#[tauri::command]
fn fs_delete(path: String) -> Result<(), String> {
    let p = validate_fs_path(&path, false)?;
    move_to_trash(&p)
}

/// Rename / move a path to a new name within the same parent directory.
#[tauri::command]
fn fs_rename(path: String, new_name: String) -> Result<(), String> {
    validate_new_leaf_name(&new_name)?;
    let src = validate_fs_path(&path, false)?;
    let dest = src
        .parent()
        .ok_or_else(|| "No parent directory".to_string())?
        .join(&new_name);
    if dest.exists() {
        return Err("Destination already exists".to_string());
    }
    std::fs::rename(&src, dest).map_err(|e| format!("Rename failed: {e}"))
}

/// Paste (copy or move) a file/directory into a target directory.
/// `action` is either "copy" or "cut".
#[tauri::command]
fn fs_paste(action: String, src_path: String, target_dir: String) -> Result<(), String> {
    let src = validate_fs_path(&src_path, false)?;
    let target_canonical = validate_fs_path(&target_dir, false)?;
    if !target_canonical.is_dir() {
        return Err("Target is not a directory".to_string());
    }
    let file_name = src.file_name().ok_or("Invalid source path")?;
    let dest = target_canonical.join(file_name);
    if dest.exists() {
        return Err("Destination already exists".to_string());
    }

    match action.as_str() {
        "cut" => std::fs::rename(&src, &dest).map_err(|e| format!("Move failed: {e}")),
        "copy" => {
            if src.is_dir() {
                if target_canonical.starts_with(&src) {
                    return Err("Cannot copy a directory into itself".to_string());
                }
                copy_dir_all(&src, &dest).map_err(|e| format!("Copy dir failed: {e}"))
            } else {
                std::fs::copy(&src, &dest)
                    .map(|_| ())
                    .map_err(|e| format!("Copy failed: {e}"))
            }
        }
        _ => Err(format!("Unknown action: {action}")),
    }
}

/// Recursively copy a directory and all its contents.
fn copy_dir_all(src: &std::path::Path, dest: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let target = dest.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod fs_paste_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_root(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        std::env::current_dir()
            .expect("test needs current dir")
            .join("target")
            .join("fs-paste-tests")
            .join(format!("{name}-{unique}"))
    }

    #[test]
    fn fs_paste_copy_dir_all_rejects_directory_into_own_descendant() {
        let root = test_root("descendant");
        let src = root.join("src");
        let child = src.join("child");
        std::fs::create_dir_all(&child).expect("create test dirs");
        std::fs::write(src.join("file.txt"), "content").expect("write source file");

        let result = fs_paste(
            "copy".to_string(),
            src.to_string_lossy().into_owned(),
            child.to_string_lossy().into_owned(),
        );

        assert!(result.is_err());
        assert!(!child.join("src").exists());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn fs_paste_copy_dir_all_copies_directory_to_sibling() {
        let root = test_root("sibling");
        let src = root.join("src");
        let target = root.join("target");
        std::fs::create_dir_all(&src).expect("create source dir");
        std::fs::create_dir_all(&target).expect("create target dir");
        std::fs::write(src.join("file.txt"), "content").expect("write source file");

        fs_paste(
            "copy".to_string(),
            src.to_string_lossy().into_owned(),
            target.to_string_lossy().into_owned(),
        )
        .expect("copy to sibling should succeed");

        assert_eq!(
            std::fs::read_to_string(target.join("src").join("file.txt")).expect("read copied file"),
            "content"
        );

        let _ = std::fs::remove_dir_all(root);
    }
}

// ─── Tier Terminal API ────────────────────────────────────────────────────────

#[tauri::command]
async fn tier_terminal_start(
    session_id: String,
    tool: Option<String>,
    tool_data: Option<String>,
    cols: u16,
    rows: u16,
    theme_mode: Option<String>,
    locale: Option<String>,
    cwd: Option<String>,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    // ── Per-pane MCP wiring for multi-agent panes ────────────────────
    // For each multi-agent pane (session id like "tab-X::pane-N")
    // running Claude, Codex, or Gemini, spawn an MCP server with that
    // pane's identity baked in and prepare per-pane CLI artifacts:
    //   - Claude: `--mcp-config <pane>/claude-mcp.json` + `--append-system-prompt`
    //   - Codex:  `-c mcp_servers.coffee-cli.url='...'` + `-c experimental_instructions_file='<pane>/inst.md'`
    // Across both allowed CLIs, `whoami()` returns the deterministic pane id,
    // `list_panes()` marks `is_self: true` on the matching row, and
    // dispatched text auto-prefixes with `[From <pane>]`. No global
    // config injection, no workspace files written, no env var that
    // would redirect the CLI's HOME and break auth.
    //
    // For other tools this stays
    // a no-op — their multi-agent participation is just "be a regular
    // PTY the user can read"; they don't get a per-pane MCP server.
    let mut pane_paths: Option<crate::mcp_injector::PaneConfigPaths> = None;
    {
        let in_multi_agent = session_id.contains("::pane-");
        let pane_cli_kind = match tool.as_deref() {
            Some(k @ ("claude" | "codex")) if in_multi_agent => Some(k),
            _ => None,
        };
        if let Some(kind) = pane_cli_kind {
            let endpoint = ensure_pane_mcp_running(&state, &session_id).await?;
            let protocol = crate::multi_agent_protocol::build_pane_system_prompt(&session_id);
            match crate::mcp_injector::prepare_pane_config_dir(
                &session_id,
                kind,
                &endpoint,
                &protocol,
            ) {
                Ok(paths) => pane_paths = Some(paths),
                Err(e) => log::warn!(
                    "[mcp] per-pane config dir for {} ({}) failed: {} \
                     — pane will run without coffee-cli MCP wiring",
                    session_id,
                    kind,
                    e
                ),
            }
        }
    }

    // Offload the whole spawn sequence to a blocking thread so the Tauri
    // command dispatcher returns immediately. Without this, Windows was
    // paying ~cmd.exe boot + Defender AV scan + Node startup on the command
    // thread, stalling every other IPC call (resize, theme, terminal I/O)
    // until the spawn returned. Running in the terminal directly avoids
    // this because no IPC layer is involved — the shell forks directly.
    let terminal_session = state.terminal_session.clone();
    tauri::async_runtime::spawn_blocking(move || {
        tier_terminal_start_blocking(
            session_id,
            tool,
            tool_data,
            cols,
            rows,
            theme_mode,
            locale,
            cwd,
            app,
            terminal_session,
            pane_paths,
        )
    })
    .await
    .map_err(|e| format!("Spawn task join failed: {e}"))?
}

fn tier_terminal_start_blocking(
    session_id: String,
    tool: Option<String>,
    tool_data: Option<String>,
    cols: u16,
    rows: u16,
    theme_mode: Option<String>,
    locale: Option<String>,
    cwd: Option<String>,
    app: tauri::AppHandle,
    terminal_session: terminal::SharedSession,
    pane_paths: Option<crate::mcp_injector::PaneConfigPaths>,
) -> Result<(), String> {
    // CWD resolution order (first non-empty wins):
    //   1. cwd passed from the frontend (launchpad's folder picker / per-tab cwd)
    //   2. tool_config.default_cwd from ~/.coffee-cli/tools.json (WSL-type users
    //      who want a fixed launch dir regardless of launchpad selection)
    //   3. empty → spawn process inherits Coffee CLI's own cwd
    //
    // The launchpad picker dominates because it's the per-launch user choice;
    // tool_config.default_cwd is the always-on fallback for users who don't
    // want to pick each time (or whose launchpad-side path can't address the
    // tool's actual workspace, e.g. WSL).
    let frontend_cwd = cwd.unwrap_or_default();
    let dir = if !frontend_cwd.is_empty() {
        std::path::PathBuf::from(frontend_cwd)
    } else if let Some(name) = tool.as_deref() {
        let cfg = crate::tool_config::get(name);
        let cfg_cwd = if crate::tool_config::validate_entry(name, &cfg).is_ok() {
            cfg.default_cwd
        } else {
            String::new()
        };
        if cfg_cwd.is_empty() {
            std::path::PathBuf::default()
        } else {
            std::path::PathBuf::from(cfg_cwd)
        }
    } else {
        std::path::PathBuf::default()
    };

    // ── Multi-agent coordination ─────────────────────────────────────────
    // Multi-agent pane session ids look like `${tabId}::pane-N`. A pane may
    // still receive per-pane MCP/context wiring so it can identify itself and
    // dispatch to sibling panes, but this local build deliberately does not
    // grant any hands-free approval mode to the spawned CLI.
    //
    // Independent-split pane ids use the `::split-N` prefix instead
    // (FourSplitGrid). Those panes are NOT orchestrated — the user is
    // watching each pane and approves tool calls themselves. Keep both
    // coordinated and independent panes in the user's normal CLI approval
    // mode; daily supervised use should prefer single-terminal or split pane.
    let in_multi_agent = session_id.contains("::pane-");

    // Map the requested tool to an actual CLI command.
    let (cmd, args): (String, Vec<String>) = match tool.as_deref() {
        Some("claude") => {
            let mut a = vec![];
            if in_multi_agent {
                // Per-pane MCP config: this Claude session points at
                // its OWN MCP server (with `self_pane_id` baked in)
                // so `whoami()` returns deterministic answers and
                // `list_panes()` marks `is_self: true` on the matching
                // row. Claude merges this on top of any user-managed
                // `~/.claude.json` mcpServers entries.
                if let Some(p) = pane_paths
                    .as_ref()
                    .and_then(|pp| pp.claude_mcp_config_path.as_ref())
                {
                    a.push("--mcp-config".to_string());
                    a.push(p.display().to_string());
                }
                // Per-pane system prompt: bake the pane id and the
                // protocol cheat sheet directly into THIS Claude
                // session's system prompt. Survives /clear and
                // /compact. Replaces writing CLAUDE.md to the
                // workspace, so multi-agent Claude users see ZERO
                // files appear in their project directory.
                a.push("--append-system-prompt".to_string());
                a.push(crate::multi_agent_protocol::build_pane_system_prompt(
                    &session_id,
                ));
            }
            ("claude".to_string(), a)
        }
        // VibeID is a skill-launcher: spawn plain `claude` binary with `/vibeid`
        // as the initial positional prompt argument. Claude Code's REPL parses
        // leading slash commands as skill invocations, so the `vibeid` skill
        // fires immediately on startup with no PTY-write hacks required.
        Some("vibeid") => ("claude".to_string(), vec!["/vibeid".to_string()]),
        // Insights pre-run: same trick as VibeID but with /insights. Used by
        // the VibeID launcher to auto-generate the usage report on first use
        // before the real VibeID tab spawns. Because this goes through the
        // Rust Command API (not a shell), Git Bash's MSYS path-conversion
        // that mangles "/insights" into "C:/Program Files/Git/insights" is
        // bypassed entirely.
        Some("insights_prerun") => ("claude".to_string(), vec!["/insights".to_string()]),
        Some("codex") => {
            let mut a = vec![];
            if in_multi_agent {
                // Per-pane MCP wiring via Codex's `-c key=value` config
                // override (it merges onto `~/.codex/config.toml` rather
                // than replacing it, so user MCP entries / API keys /
                // auth all stay live). Two pairs:
                //   `mcp_servers.coffee-cli.url='<per-pane-url>'`
                //   `model_instructions_file='<pane-temp>/inst.md'`
                // The instructions file holds the multi-agent protocol
                // body (same text Claude gets via --append-system-prompt)
                // and Codex bakes it into the model's session context.
                // Both the URL and the instructions path are unique per
                // pane, so 4× same-CLI panes still get distinct identity.
                if let Some(extra) = pane_paths.as_ref().map(|pp| pp.codex_extra_args.clone()) {
                    a.extend(extra);
                }
            }
            ("codex".to_string(), a)
        }
        Some("qwen" | "hermes" | "opencode" | "openclaw" | "gemini") => {
            return Err(
                "This local safe build only enables Claude Code and Codex CLI agents".to_string(),
            );
        }
        Some("remote") => {
            // Parse connection info from toolData JSON
            let data = tool_data.as_deref().unwrap_or("{}");
            let conn: serde_json::Value = serde_json::from_str(data)
                .map_err(|e| format!("Invalid remote connection data: {}", e))?;

            let protocol = conn["protocol"].as_str().unwrap_or("ssh");
            let host = conn["host"].as_str().unwrap_or("localhost");
            let port = conn["port"]
                .as_u64()
                .unwrap_or(if protocol == "ssh" { 22 } else { 7681 });
            let username = conn["username"].as_str().unwrap_or("root");

            if protocol == "ssh" {
                if host.contains('\0') || username.contains('\0') || username.contains('@') {
                    return Err("Invalid SSH host or username".to_string());
                }
                if port == 0 || port > 65535 {
                    return Err("Invalid SSH port".to_string());
                }
                // Build SSH command with normal OpenSSH host-key verification.
                // Do not pass passwords on argv or disable host-key checks.
                let ssh_args = vec![
                    "-o".to_string(),
                    "BatchMode=no".to_string(),
                    "-p".to_string(),
                    port.to_string(),
                    format!("{}@{}", username, host),
                ];
                ("ssh".to_string(), ssh_args)
            } else {
                // WebSocket protocol — not handled by PTY backend
                // Frontend will handle this via xterm.js AttachAddon directly
                return Err("ws".to_string());
            }
        }

        Some("terminal") | None => {
            if cfg!(target_os = "windows") {
                ("powershell.exe".to_string(), vec!["-NoExit".to_string()])
            } else {
                ("bash".to_string(), vec!["-l".to_string(), "-i".to_string()])
            }
        }
        Some(other) => return Err(format!("Unknown tool: {other}")),
    };

    // If a session with the same ID already exists (e.g. restart-in-place),
    // forcefully kill and remove it before spawning a fresh one.
    {
        let mut lock = terminal_session.lock().unwrap();
        if let Some(old_session) = lock.remove(&session_id) {
            eprintln!(
                "[Tier Terminal] Killing existing session {} for restart",
                session_id
            );
            let _ = old_session.kill_tx.send(());
            // Brief pause to let the OS reclaim PTY resources
            drop(lock);
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }

    // Determine the CWD to pass to the Agent:
    // 1. If workspace has an explicit dir (from open-folder or resume) → use it
    // 2. Otherwise default to user's home dir (matches most agents' default)
    let spawn_cwd = if dir.as_os_str().is_empty() || !dir.is_dir() {
        dirs::home_dir().map(|p| p.to_string_lossy().to_string())
    } else {
        Some(dir.to_string_lossy().to_string())
    };

    let tool_name = tool.clone();
    let actual_cwd = spawn_cwd.clone().unwrap_or_default();

    eprintln!(
        "[Tier Terminal] Starting tool={:?}, cmd={}, arg_count={}, cwd={:?}",
        tool,
        cmd,
        args.len(),
        spawn_cwd
    );

    terminal::spawn(
        app.clone(),
        session_id.clone(),
        terminal_session.clone(),
        cmd,
        args,
        spawn_cwd,
        locale.clone().unwrap_or_else(|| "en".to_string()),
        cols,
        rows,
        tool_name.clone(),
        theme_mode,
        locale,
    )
    .map_err(|e| format!("Failed to spawn PTY: {}", e))?;

    // Emit the initial CWD to the frontend so the left panel can map immediately.
    // On Windows, cmd.exe does not emit OSC 7, and full-screen agents enter alt-screen
    // before any shell prompt appears. This one-time emit bridges the gap.
    if !actual_cwd.is_empty() {
        #[derive(serde::Serialize, Clone)]
        struct CwdPayload {
            id: String,
            cwd: String,
        }
        let _ = app.emit(
            "tier-terminal-cwd",
            CwdPayload {
                id: session_id,
                cwd: actual_cwd,
            },
        );
    }

    Ok(())
}

#[tauri::command]
fn tier_terminal_input(
    session_id: String,
    data: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    // Step 1: grab Arc handles while holding the map lock (cheap clones, no IO)
    let (writer_arc, activity_arc) = {
        let map = state.terminal_session.lock().unwrap();
        match map.get(&session_id) {
            Some(s) => (s.writer_lock.clone(), s.activity.clone()),
            None => return Err(format!("No active terminal session for id: {}", session_id)),
        }
    };
    // Map lock released — other tabs can now proceed concurrently

    // Step 2: PTY write (syscall, may block under back-pressure)
    use std::io::Write;
    let mut w = writer_arc
        .lock()
        .map_err(|e| format!("Writer lock poisoned: {}", e))?;
    w.write_all(data.as_bytes())
        .map_err(|e| format!("Write failed: {}", e))?;
    w.flush().map_err(|e| format!("Flush failed: {}", e))?;
    drop(w);

    // Step 3: Dual-signal — detect user prompt submission
    // Only trigger "working" when user presses Enter while agent is at prompt.
    if data.contains('\r') || data.contains('\n') {
        if let Ok(mut act) = activity_arc.lock() {
            if act.last_status == "wait_input" {
                act.user_submitted_at = Some(std::time::Instant::now());
            }
        }
    }

    Ok(())
}

#[tauri::command]
fn tier_terminal_kill(session_id: String, state: State<'_, AppState>) -> Result<(), String> {
    let map = state.terminal_session.lock().unwrap();
    if let Some(session) = map.get(&session_id) {
        let _ = session.kill_tx.send(());
    }
    Ok(())
}

#[tauri::command]
fn tier_terminal_resize(
    session_id: String,
    cols: u16,
    rows: u16,
    state: State<'_, AppState>,
) -> Result<(), String> {
    use portable_pty::PtySize;
    let map = state.terminal_session.lock().unwrap();
    if let Some(session) = map.get(&session_id) {
        let master_guard = session._master.lock().unwrap();
        if let Some(ref master) = *master_guard {
            let size = PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            };
            master
                .resize(size)
                .map_err(|e| format!("Resize failed: {}", e))?;
        }
    }
    Ok(())
}

// ─── Session Resume API ──────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone)]
struct SavedSession {
    id: String,
    name: String,
    tool: String,
    cwd: String,
    session_token: Option<String>,
    saved_at: String,
    file_path: Option<String>,
    turn_count: Option<u32>,
}

fn sessions_file_path() -> PathBuf {
    let dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".coffee-cli");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("sessions.json")
}

/// XML-style tags injected into the user message stream by Claude /
/// Codex when integrated with an IDE or shell. These are
/// not things the user typed — filtering them out of the history
/// title extractor keeps the sidebar readable (no more
/// "<ide_opened_file>The user opened the..." or "# AGENTS.md
/// instructions for ..." cards).
const SYSTEM_INJECTION_TAGS: &[&str] = &[
    "<environment_context>",
    "<ide_opened_file>",
    "<ide_closed_file>",
    "<ide_selection>",
    "<system-reminder>",
    "<command-message>",
    "<command-name>",
    // Codex injects the contents of `AGENTS.md` (project) and any
    // pre-v1.5 Coffee-CLI workspace pointer as a synthetic user
    // message at session start.
    "# AGENTS.md",
];

fn is_system_injected(text: &str) -> bool {
    let t = text.trim();
    SYSTEM_INJECTION_TAGS.iter().any(|tag| t.starts_with(tag))
}

fn parse_agent_jsonl(file_path: &std::path::Path, tool_name: &str) -> Option<SavedSession> {
    use std::io::BufRead;
    let file = std::fs::File::open(file_path).ok()?;
    let reader = std::io::BufReader::new(file);

    let mut session_id = file_path.file_stem()?.to_string_lossy().to_string();
    let mut cwd = String::new();
    let mut updated_at = String::new();
    let mut title = String::new();
    let mut total_messages = 0;

    for line in reader.lines().map_while(Result::ok) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
            if let Some(s) = value.get("sessionId").and_then(|v| v.as_str()) {
                if !s.is_empty() {
                    session_id = s.to_string();
                }
            }
            if let Some(c) = value.get("cwd").and_then(|v| v.as_str()) {
                if cwd.is_empty() && !c.is_empty() {
                    cwd = c.to_string();
                }
            }
            let mut maybe_msg_obj = value.get("message").and_then(|v| v.as_object());
            if maybe_msg_obj.is_none() {
                if let Some(payload) = value.get("payload").and_then(|v| v.as_object()) {
                    if let Some(ptype) = payload.get("type").and_then(|v| v.as_str()) {
                        if ptype == "message" {
                            maybe_msg_obj = Some(payload);
                        }
                    }
                }
            }

            if let Some(msg_obj) = maybe_msg_obj {
                if let Some(role) = msg_obj.get("role").and_then(|v| v.as_str()) {
                    if role == "user" || role == "assistant" {
                        total_messages += 1;
                    }
                    if role == "user" && title.is_empty() {
                        if let Some(content_str) = msg_obj.get("content").and_then(|v| v.as_str()) {
                            // Skip whole-message IDE/system injections so the
                            // next real user line becomes the title.
                            if !is_system_injected(content_str) {
                                let content_safe = content_str.replace('\n', " ");
                                let mut chars = content_safe.chars();
                                let t: String = chars.by_ref().take(40).collect();
                                title = if chars.next().is_some() {
                                    format!("{}...", t)
                                } else {
                                    t
                                };
                            }
                        } else if let Some(content_arr) =
                            msg_obj.get("content").and_then(|v| v.as_array())
                        {
                            // Extract text from object array
                            for block in content_arr {
                                if let Some(t) = block.get("type").and_then(|v| v.as_str()) {
                                    if t == "text" || t == "input_text" {
                                        if let Some(text) =
                                            block.get("text").and_then(|v| v.as_str())
                                        {
                                            if is_system_injected(text) {
                                                continue; // skip IDE / system-injected prompts
                                            }
                                            let safe_text = text.replace('\n', " ");
                                            let mut chars = safe_text.chars();
                                            let chunk: String = chars.by_ref().take(40).collect();
                                            title = if chars.next().is_some() {
                                                format!("{}...", chunk)
                                            } else {
                                                chunk
                                            };
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Fallback: If cwd is still empty, derive it from the parent project folder name
    // e.g., "D--Coffee-Code" -> "D:\Coffee-Code", "C--Users--..." -> "C:\Users\..."
    if cwd.is_empty() {
        if let Some(parent) = file_path.parent() {
            if let Some(folder_name) = parent.file_name().and_then(|n| n.to_str()) {
                if folder_name.contains("--") {
                    let mut parts = folder_name.split("--");
                    if let Some(drive) = parts.next() {
                        let rest: Vec<&str> = parts.collect();
                        let decoded_path = if cfg!(target_os = "windows") {
                            format!("{}:\\{}", drive, rest.join("\\"))
                        } else {
                            format!("/{}/{}", drive, rest.join("/"))
                        };
                        cwd = decoded_path;
                    }
                }
            }
        }
    }
    let turn_count = if total_messages > 0 {
        std::cmp::max(1, (total_messages + 1) / 2)
    } else {
        0
    };

    // Fallback date from file metadata
    if let Ok(meta) = std::fs::metadata(file_path) {
        if let Ok(mod_time) = meta.modified() {
            if let Ok(dur) = mod_time.duration_since(std::time::SystemTime::UNIX_EPOCH) {
                updated_at = dur.as_millis().to_string();
            }
        }
    }

    if title.is_empty() {
        let mut chars = tool_name.chars();
        let cap_name = match chars.next() {
            None => String::new(),
            Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
        };
        title = format!("{} Session", cap_name);
    }

    Some(SavedSession {
        id: format!("{}_native_{}", tool_name, session_id),
        name: title,
        tool: tool_name.to_string(),
        cwd,
        session_token: Some(session_id),
        saved_at: updated_at,
        file_path: Some(file_path.to_string_lossy().into_owned()),
        turn_count: Some(turn_count),
    })
}

/// Codex CLI sessions live at
/// `~/.codex/sessions/<YYYY>/<MM>/<DD>/rollout-<ts>-<uuid>.jsonl`.
/// Schema:
///   - first row: `{type: "session_meta", payload: {id, cwd, originator: "codex-tui", ...}}`
///   - subsequent rows: `{type: "response_item", payload: {type: "message", role, content: [{type: "input_text", text}]}}`
///     (also `user_message`, `event_msg`, `turn_context`, etc. — we ignore the non-message ones)
fn parse_codex_session_jsonl(file_path: &std::path::Path) -> Option<SavedSession> {
    use std::io::BufRead;
    let file = std::fs::File::open(file_path).ok()?;
    let reader = std::io::BufReader::new(file);

    let mut session_id = file_path.file_stem()?.to_string_lossy().to_string();
    let mut cwd = String::new();
    let mut updated_at = String::new();
    let mut title = String::new();
    let mut total_messages = 0;

    for line in reader.lines().map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let row_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let payload = match value.get("payload") {
            Some(p) => p,
            None => continue,
        };

        // Session meta: pull id + cwd off the first row.
        if row_type == "session_meta" {
            if let Some(id) = payload.get("id").and_then(|v| v.as_str()) {
                if !id.is_empty() {
                    session_id = id.to_string();
                }
            }
            if let Some(c) = payload.get("cwd").and_then(|v| v.as_str()) {
                if !c.is_empty() {
                    cwd = c.to_string();
                }
            }
            continue;
        }

        // Message rows: response_item with payload.type=message, or
        // the dedicated user_message row type. Both wrap content as
        // an array of `{type: "input_text", text}` blocks.
        let payload_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let is_msg = (row_type == "response_item" && payload_type == "message")
            || row_type == "user_message";
        if !is_msg {
            continue;
        }
        let role = payload.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role == "user" || role == "assistant" {
            total_messages += 1;
        }
        if !title.is_empty() || role != "user" {
            continue;
        }
        let Some(content_arr) = payload.get("content").and_then(|v| v.as_array()) else {
            continue;
        };
        for block in content_arr {
            let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if block_type != "input_text" && block_type != "text" {
                continue;
            }
            let Some(text) = block.get("text").and_then(|v| v.as_str()) else {
                continue;
            };
            if is_system_injected(text) {
                continue; // skip AGENTS.md / environment_context wrappers
            }
            let safe = text.replace('\n', " ");
            let mut chars = safe.chars();
            let chunk: String = chars.by_ref().take(40).collect();
            title = if chars.next().is_some() {
                format!("{}...", chunk)
            } else {
                chunk
            };
            break;
        }
    }

    if let Ok(meta) = std::fs::metadata(file_path) {
        if let Ok(mod_time) = meta.modified() {
            if let Ok(dur) = mod_time.duration_since(std::time::SystemTime::UNIX_EPOCH) {
                updated_at = dur.as_millis().to_string();
            }
        }
    }
    if title.is_empty() {
        title = "Codex Session".to_string();
    }
    let turn_count = if total_messages > 0 {
        std::cmp::max(1, (total_messages + 1) / 2)
    } else {
        0
    };

    Some(SavedSession {
        id: format!("codex_native_{}", session_id),
        name: title,
        tool: "codex".to_string(),
        cwd,
        session_token: Some(session_id),
        saved_at: updated_at,
        file_path: Some(file_path.to_string_lossy().into_owned()),
        turn_count: Some(turn_count),
    })
}

#[tauri::command]
fn read_native_session(file_path: String) -> Result<String, String> {
    let path = std::path::Path::new(&file_path);

    // Only allow .jsonl / .json files
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext != "jsonl" && ext != "json" {
        return Err("Only .jsonl and .json files are allowed".to_string());
    }

    // Canonicalize to resolve any `..` or symlink traversal
    let canonical_raw = path
        .canonicalize()
        .map_err(|e| format!("Invalid path: {e}"))?;
    // On Windows, canonicalize() prepends \\?\ (UNC extended-length prefix).
    // Strip it so that starts_with() comparisons against plain home-dir paths work.
    #[cfg(windows)]
    let canonical = {
        let s = canonical_raw.to_string_lossy();
        if s.starts_with(r"\\?\") {
            std::path::PathBuf::from(s[4..].to_string())
        } else {
            canonical_raw
        }
    };
    #[cfg(not(windows))]
    let canonical = canonical_raw;

    // Must reside under a known agent data directory.
    //
    // Built-in defaults cover the standard install locations; any
    // additional paths the user configured via tool_config.history_path
    // are also allowed (otherwise the WSL-redirected scanner would find
    // sessions but reading them back would 403). Path canonicalization
    // already resolved symlinks, so this is a pure prefix check.
    let home = dirs::home_dir().ok_or("Cannot determine home directory")?;
    let mut allowed: Vec<std::path::PathBuf> =
        vec![home.join(".claude"), home.join(".codex").join("sessions")];
    for tool in ["claude", "codex"] {
        let cfg = crate::tool_config::get(tool).history_path;
        if let Some(root) = crate::tool_config::validated_history_path(tool, &cfg) {
            allowed.push(root);
        }
    }
    if !allowed.iter().any(|prefix| canonical.starts_with(prefix)) {
        return Err("Access denied: path is outside allowed agent data directories".to_string());
    }

    std::fs::read_to_string(&canonical).map_err(|e| e.to_string())
}

fn collect_jsonl_paths_with_mtime(
    dir: std::path::PathBuf,
    depth: u8,
    tool: &'static str,
    out: &mut Vec<(std::time::SystemTime, std::path::PathBuf, &'static str)>,
) {
    if depth == 0 || !dir.is_dir() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    let mtime = entry
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                    out.push((mtime, path, tool));
                }
            } else if path.is_dir() {
                collect_jsonl_paths_with_mtime(path, depth - 1, tool, out);
            }
        }
    }
}

#[tauri::command]
async fn get_native_history() -> Result<Vec<SavedSession>, String> {
    // Async command + spawn_blocking so the file I/O runs on a dedicated
    // blocking thread pool and never blocks the Tauri command dispatcher.
    // Other IPC calls (resize, theme switches, etc.) stay responsive while
    // history is being scanned on app startup.
    tauri::async_runtime::spawn_blocking(load_native_history_blocking)
        .await
        .map_err(|e| format!("History task join failed: {e}"))?
}

fn load_native_history_blocking() -> Result<Vec<SavedSession>, String> {
    // Cap history to the N most recent entries. Keeps UI responsive when users
    // have hundreds of sessions — parsing a full jsonl/json file is expensive,
    // so we pre-select candidates by file mtime and only parse the top N.
    const HISTORY_LIMIT: usize = 30;

    let mut file_candidates: Vec<(std::time::SystemTime, std::path::PathBuf, &'static str)> =
        Vec::new();
    let mut result: Vec<SavedSession> = Vec::new();

    if let Some(home) = dirs::home_dir() {
        // Each scanner can be redirected at a custom directory via
        // ~/.coffee-cli/tools.json (`tool_config.history_path`). Lets
        // WSL users point Coffee CLI at their `\\wsl.localhost\<distro>
        // \home\<user>\.<tool>\sessions\` paths, conda users point at
        // their env-specific session dirs, etc. Falls back to the
        // built-in default when no override is set.

        // 1. Claude Code (depth 2: projects/<hash>/<hash>.jsonl)
        let claude_dir =
            crate::tool_config::history_path_for("claude", home.join(".claude").join("projects"));
        collect_jsonl_paths_with_mtime(claude_dir, 2, "claude", &mut file_candidates);

        // 2. Codex (depth 4: sessions/<YYYY>/<MM>/<DD>/rollout-*.jsonl)
        let codex_dir =
            crate::tool_config::history_path_for("codex", home.join(".codex").join("sessions"));
        collect_jsonl_paths_with_mtime(codex_dir, 4, "codex", &mut file_candidates);
    }

    // Sort candidates by mtime desc and parse only the newest HISTORY_LIMIT.
    file_candidates.sort_by(|a, b| b.0.cmp(&a.0));
    file_candidates.truncate(HISTORY_LIMIT);

    for (_, path, tool) in &file_candidates {
        let parsed = match *tool {
            "codex" => parse_codex_session_jsonl(path),
            other => parse_agent_jsonl(path, other),
        };
        if let Some(session) = parsed {
            result.push(session);
        }
    }
    result.sort_by(|a, b| b.saved_at.cmp(&a.saved_at));
    result.truncate(HISTORY_LIMIT);
    Ok(result)
}

#[tauri::command]
fn tier_terminal_resume(
    session_id: String,
    saved_session_id: String, // The UUID of the new terminal tab
    tool: String,
    session_token: String,
    cols: u16,
    rows: u16,
    cwd: String,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let preset = terminal::find_preset(&tool).ok_or_else(|| format!("Unknown tool: {}", tool))?;
    let resume_program = preset
        .resume_program
        .ok_or_else(|| format!("Tool '{}' does not support resume", tool))?;

    // Validate session_token against the tool's token_format.
    // Prevents flag injection: "uuid --dangerously-skip-permissions" would fail this check.
    if let Some(fmt) = preset.token_format {
        let re =
            regex::Regex::new(fmt).map_err(|e| format!("Invalid token format pattern: {e}"))?;
        if !re.is_match(&session_token) {
            return Err(format!("Invalid session token format for tool '{}'", tool));
        }
    }

    // Build args without string interpolation: token is always a separate element,
    // never concatenated into a command string that gets split by whitespace.
    let program = resume_program.to_string();
    let mut args: Vec<String> = preset
        .resume_args_before
        .iter()
        .map(|s| s.to_string())
        .collect();
    args.push(session_token.clone());
    args.extend(preset.resume_args_after.iter().map(|s| s.to_string()));

    let actual_cwd = cwd.clone();
    let emit_session_id = saved_session_id.clone();

    terminal::spawn(
        app.clone(),
        saved_session_id,
        state.terminal_session.clone(),
        program,
        args,
        Some(cwd),
        "en".to_string(),
        cols,
        rows,
        Some(tool),
        None, // theme_mode: resume sessions use default detection
        None, // locale: resume sessions use env detection
    )
    .map_err(|e| format!("Failed to resume: {}", e))?;

    // Emit CWD so the left panel maps the resumed session's directory
    if !actual_cwd.is_empty() {
        #[derive(serde::Serialize, Clone)]
        struct CwdPayload {
            id: String,
            cwd: String,
        }
        let _ = app.emit(
            "tier-terminal-cwd",
            CwdPayload {
                id: emit_session_id,
                cwd: actual_cwd,
            },
        );
    }

    // Remove from saved sessions file
    let path = sessions_file_path();
    if path.exists() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(mut saved) = serde_json::from_str::<Vec<SavedSession>>(&content) {
                saved.retain(|s| s.id != session_id);
                let _ = std::fs::write(
                    &path,
                    serde_json::to_string_pretty(&saved).unwrap_or_default(),
                );
            }
        }
    }

    Ok(())
}

// ─── Coffee Play (Arcade) ────────────────────────────────────────────────────

#[derive(Serialize)]
struct JsdosBundle {
    name: String,
    path: String,
    size: u64,
}

fn jsdos_play_dirs() -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".coffee-cli").join("play"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            candidates.push(parent.join("play"));
        }
    }
    candidates.push(PathBuf::from("play"));
    candidates.push(PathBuf::from("src-ui/public/play"));
    candidates
}

fn validate_jsdos_name(name: &str) -> Result<&str, String> {
    validate_new_leaf_name(name)?;
    if !name.ends_with(".jsdos") {
        return Err("Bundle name must end with .jsdos".to_string());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        return Err("Bundle name contains unsupported characters".to_string());
    }
    Ok(name)
}

fn validate_jsdos_read_path(path: &str) -> Result<PathBuf, String> {
    let p = canonical_existing(Path::new(path))?;
    if p.extension().and_then(|e| e.to_str()) != Some("jsdos") {
        return Err("Not a .jsdos file".to_string());
    }
    for dir in jsdos_play_dirs() {
        if let Ok(root) = dir.canonicalize().map(normalize_canonical) {
            if p.starts_with(root) {
                return Ok(p);
            }
        }
    }
    Err("Access denied: bundle is outside Coffee Play directories".to_string())
}

/// List all .jsdos game bundles in the `play` directory next to the executable
/// (or in the project root during development).
#[tauri::command]
fn list_jsdos_bundles() -> Vec<JsdosBundle> {
    let mut bundles = Vec::new();

    for play_dir in jsdos_play_dirs() {
        if !play_dir.is_dir() {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(play_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e.to_ascii_lowercase())
                    == Some(std::ffi::OsString::from("jsdos"))
                {
                    if let Ok(meta) = entry.metadata() {
                        bundles.push(JsdosBundle {
                            name: entry.file_name().to_string_lossy().to_string(),
                            path: path.to_string_lossy().to_string(),
                            size: meta.len(),
                        });
                    }
                }
            }
        }
        if !bundles.is_empty() {
            break;
        } // Use first directory that has games
    }

    bundles
}

/// Read a .jsdos bundle file and return its raw bytes.
/// This allows the frontend to load local bundles without asset protocol.
#[tauri::command]
fn read_jsdos_bundle(path: String) -> Result<Vec<u8>, String> {
    let p = validate_jsdos_read_path(&path)?;
    std::fs::read(p).map_err(|e| format!("Failed to read: {}", e))
}

/// Save a downloaded .jsdos bundle to the local play directory
#[tauri::command]
fn save_jsdos_bundle(name: String, data: Vec<u8>) -> Result<(), String> {
    const MAX_JSDOS_BUNDLE_BYTES: usize = 256 * 1024 * 1024;
    let safe_name = validate_jsdos_name(&name)?;
    if data.len() > MAX_JSDOS_BUNDLE_BYTES {
        return Err("Bundle too large".to_string());
    }
    let play_dir = dirs::home_dir()
        .ok_or_else(|| "Could not find home directory".to_string())?
        .join(".coffee-cli")
        .join("play");

    if !play_dir.exists() {
        std::fs::create_dir_all(&play_dir).map_err(|e| e.to_string())?;
    }

    let file_path = play_dir.join(safe_name);
    std::fs::write(file_path, data).map_err(|e| e.to_string())
}

// ─── Task Board Persistence ──────────────────────────────────────────────────

fn tasks_file_path() -> PathBuf {
    let dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".coffee-cli");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("tasks.json")
}

#[tauri::command]
fn load_tasks() -> Result<String, String> {
    let path = tasks_file_path();
    if path.exists() {
        std::fs::read_to_string(&path).map_err(|e| format!("Failed to read tasks: {}", e))
    } else {
        Ok("[]".to_string())
    }
}

#[tauri::command]
fn save_tasks(data: String, app: tauri::AppHandle) -> Result<(), String> {
    // Validate JSON before writing to disk — guards against corrupted broadcasts
    serde_json::from_str::<serde_json::Value>(&data)
        .map_err(|e| format!("Invalid task data (not valid JSON): {e}"))?;
    let path = tasks_file_path();
    std::fs::write(&path, &data).map_err(|e| format!("Failed to save tasks: {}", e))?;
    // Notify all windows so other instances can reload
    let _ = app.emit("tasks-changed", &data);
    Ok(())
}

#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    let trimmed = url.trim();
    if trimmed.len() > 4096
        || trimmed.contains('\0')
        || trimmed.contains('\n')
        || trimmed.contains('\r')
    {
        return Err("Invalid URL".to_string());
    }
    let Some(rest) = trimmed.strip_prefix("https://coffeecli.com") else {
        return Err("Unsupported URL".to_string());
    };
    if !(rest.is_empty() || rest.starts_with('/') || rest.starts_with('?') || rest.starts_with('#'))
    {
        return Err("Unsupported URL".to_string());
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("rundll32.exe")
            .arg("url.dll,FileProtocolHandler")
            .arg(trimmed)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| format!("Failed to open URL: {e}"))?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(trimmed)
            .spawn()
            .map_err(|e| format!("Failed to open URL: {e}"))?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(trimmed)
            .spawn()
            .map_err(|e| format!("Failed to open URL: {e}"))?;
    }
    Ok(())
}

// ─── Per-tool launch overrides (~/.coffee-cli/tools.json) ────────────────
//
// Lets users tell Coffee CLI things like "my claude is at `wsl
// ~/.local/bin/claude`, not on PATH". Replaces what the abandoned in-app
// installer was supposed to handle by auto-detection — defer to the user,
// who knows their machine better than we do.

#[tauri::command]
pub fn get_tool_config(tool: String) -> crate::tool_config::ToolConfigEntry {
    crate::tool_config::get(&tool)
}

#[tauri::command]
pub fn get_all_tool_configs() -> crate::tool_config::ToolConfig {
    crate::tool_config::load()
}

#[tauri::command]
pub fn set_tool_config(
    tool: String,
    entry: crate::tool_config::ToolConfigEntry,
) -> Result<(), String> {
    crate::tool_config::validate_entry(&tool, &entry)?;
    crate::tool_config::set(&tool, entry).map_err(|e| e.to_string())
}

#[derive(Serialize)]
pub struct MultiAgentModeReport {
    pub ok: bool,
    pub warnings: Vec<String>,
}

/// Snapshot the current MCP topology to `~/.coffee-cli/mcp-state.json`
/// so the `coffee-cli mcp-status` subcommand can read it from any
/// terminal. Called after every successful per-pane MCP spawn. The
/// `anonymous` slot in the manifest is always `None` post-v1.5: every
/// multi-agent pane has its own `self_pane_id`-baked server now, no
/// shared anonymous endpoint exists.
async fn refresh_mcp_state_manifest(state: &AppState) {
    let panes_snapshot = state.pane_mcp_endpoints.lock().await.clone();
    crate::mcp_server::write_state_manifest(None, &panes_snapshot);
}

/// Lazy-spawn a PER-PANE MCP server bound to a specific pane id, on
/// its own dedicated port. Idempotent — if a server already exists
/// for `pane_id`, returns it. This is how a multi-agent Claude Code
/// pane gets a unique MCP endpoint with its own identity baked in:
/// every call to its `whoami()` returns the same `pane_id`,
/// `list_panes` marks the matching row `is_self: true`, and
/// `send_to_pane` prefixes dispatched text with `[From <pane_id>]`.
///
/// Uses `mcp_spawn_lock` to serialize concurrent first-callers for
/// the same pane id so we never bind two listeners.
pub async fn ensure_pane_mcp_running(
    state: &AppState,
    pane_id: &str,
) -> Result<crate::mcp_server::McpEndpoint, String> {
    // Fast path — already spawned for this pane.
    {
        let guard = state.pane_mcp_endpoints.lock().await;
        if let Some(ep) = guard.get(pane_id) {
            return Ok(ep.clone());
        }
    }

    // Slow path — take the global spawn lock, double-check, then bind.
    let _spawn_guard = state.mcp_spawn_lock.lock().await;
    {
        let guard = state.pane_mcp_endpoints.lock().await;
        if let Some(ep) = guard.get(pane_id) {
            return Ok(ep.clone());
        }
    }

    let panes = std::sync::Arc::new(crate::mcp_server::PaneStore::new(
        state.terminal_session.clone(),
    ));
    let endpoint = crate::mcp_server::spawn(panes, Some(pane_id.to_string()))
        .await
        .map_err(|e| format!("mcp spawn for {}: {}", pane_id, e))?;
    log::info!(
        "[mcp] per-pane server up at {} (pane={})",
        endpoint.url,
        pane_id
    );

    state
        .pane_mcp_endpoints
        .lock()
        .await
        .insert(pane_id.to_string(), endpoint.clone());
    refresh_mcp_state_manifest(state).await;
    Ok(endpoint)
}

/// Enable multi-agent mode for the given workspace.
///
/// Post-v1.5 this is a thin handshake — per-pane MCP servers and
/// per-pane CLI artifacts (Claude `mcp.json` / Codex `instructions.md`) are
/// created lazily inside `tier_terminal_start` when each pane spawns its CLI.
/// No workspace files are written, no global `~/.codex` `mcp_servers` blocks
/// get injected, no env var redirects the CLI's HOME (so auth stays live).
///
/// The frontend still calls this on tab mount as a structured place
/// for cross-cutting validation (workspace must exist, future license
/// gating, etc.) — kept around for that hook, not because it does any
/// heavy lifting today.
///
/// `_tools` and `_state` are kept in the signature for API
/// compatibility with the existing TS `commands.enableMultiAgentMode`
/// invocation; they're unused here.
#[tauri::command]
async fn enable_multi_agent_mode(
    workspace: String,
    _tools: Vec<String>,
    _state: tauri::State<'_, AppState>,
) -> Result<MultiAgentModeReport, String> {
    let ws = PathBuf::from(&workspace);
    if !ws.is_dir() {
        return Err(format!("workspace is not a directory: {}", workspace));
    }
    Ok(MultiAgentModeReport {
        ok: true,
        warnings: Vec::new(),
    })
}

/// Disable multi-agent mode for the given workspace.
///
/// Post-v1.5 this is a no-op for the workspace itself — multi-agent
/// mode no longer writes any workspace files or global config entries
/// to clean up here. Per-pane MCP servers and their temp artifacts
/// persist for the app's lifetime (they live under
/// `<temp>/coffee-cli/panes/` and are pruned by
/// `mcp_injector::prune_pane_artifacts()` at the next launch and at app
/// shutdown).
///
/// `_workspace` is kept in the signature for API compat with the TS
/// caller in `MultiAgentGrid.tsx`'s unmount cleanup.
#[tauri::command]
fn disable_multi_agent_mode(_workspace: String) -> Result<MultiAgentModeReport, String> {
    Ok(MultiAgentModeReport {
        ok: true,
        warnings: Vec::new(),
    })
}

pub fn start_ui() -> anyhow::Result<()> {
    // Drop the previous run's per-pane artifacts before we boot —
    // `<temp>/coffee-cli/panes/*` artifacts from a crashed or hard-killed
    // prior session would otherwise accumulate. New artifacts are recreated
    // lazily by `tier_terminal_start` as multi-agent panes spawn.
    crate::mcp_injector::prune_pane_artifacts();

    // Create shared session BEFORE the builder so we can clone it for the exit handler
    let terminal_session = terminal::SharedSession::default();
    let hook_token = uuid::Uuid::new_v4().to_string();

    tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .manage(AppState {
            terminal_session,
            hook_port: std::sync::atomic::AtomicU16::new(0),
            hook_token,
            fs_watcher: Mutex::new(None),
            pane_mcp_endpoints: tokio::sync::Mutex::new(
                std::collections::HashMap::new(),
            ),
            mcp_spawn_lock: tokio::sync::Mutex::new(()),
        })
        .invoke_handler(tauri::generate_handler![
            pick_folder,
            window_minimize,
            window_maximize,
            window_close,
            show_main_window,
            tier_terminal_start,
            tier_terminal_input,
            tier_terminal_kill,
            tier_terminal_resize,
            tier_terminal_resume,
            get_native_history,
            read_native_session,
            check_tools_installed,
            start_fs_watcher,
            stop_fs_watcher,
            save_clipboard_image,
            list_drives,
            list_directory,
            show_in_folder,
            fs_delete,
            fs_rename,
            fs_paste,
            list_jsdos_bundles,
            read_jsdos_bundle,
            save_jsdos_bundle,
            load_tasks,
            save_tasks,
            open_url,
            check_skill_installed,
            install_vibeid_skill,
            check_vibeid_report_exists,
            check_vibeid_report_mtime,
            enable_multi_agent_mode,
            disable_multi_agent_mode,
            get_tool_config,
            get_all_tool_configs,
            set_tool_config,
        ])
        .setup(|app| {
            // Global Claude Code hooks mutate user configuration. Keep that
            // integration opt-in so app launch never silently patches
            // ~/.claude/settings.json.
            if std::env::var("COFFEE_CLI_ENABLE_CLAUDE_HOOKS").as_deref() == Ok("1") {
                crate::hook_installer::install_all();
            } else {
                log::info!(
                    "[hook-installer] skipped; set COFFEE_CLI_ENABLE_CLAUDE_HOOKS=1 to opt in"
                );
            }

            // Start loopback TCP listener that receives events from the hook
            // script and forwards them to the frontend as `agent-status` events.
            let token = app.state::<AppState>().hook_token.clone();
            match crate::hook_server::start(app.handle().clone(), token) {
                Ok(port) => {
                    app.state::<AppState>()
                        .hook_port
                        .store(port, std::sync::atomic::Ordering::SeqCst);
                }
                Err(e) => {
                    eprintln!("[hook-server] start failed: {}", e);
                }
            }

            // Per-pane MCP servers are spawned lazily inside
            // `tier_terminal_start` when each multi-agent pane boots
            // its CLI. Users who never open a multi-agent tab pay
            // zero MCP cost.

            // ── Bulletproof window-reveal fallback ──────────────────
            // The window is created with `visible: false` so the user
            // never sees the platform's chrome flash — main.tsx
            // invokes `show_main_window` after the first paint via
            // double-RAF and the window appears already-themed.
            //
            // BUT: if the WebView never paints (Gatekeeper rejection
            // on adhoc-signed macOS bundles, WebKit2GTK Wayland blank
            // window on Ubuntu 24.04, or any JS error before
            // ReactDOM mount), the `invoke` never fires, the window
            // stays hidden forever, and users see "process is running,
            // hook-server is listening, but there is no window".
            // Multiple users have hit this across both platforms.
            //
            // Force a reveal after 3s as a safety net. Healthy
            // startups call show_main_window in ~50ms, well before
            // this fires, so the no-flash UX is preserved. Broken
            // startups at least get a (possibly blank) window the
            // user can interact with — they can quit it, file a bug
            // with devtools, or report what they see, instead of
            // staring at nothing.
            {
                use tauri::Manager;
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(3));
                    if let Some(window) = handle.get_webview_window("main") {
                        if !window.is_visible().unwrap_or(false) {
                            eprintln!(
                                "[main-window] frontend never called show_main_window after 3s — forcing reveal (likely WebView render failure)"
                            );
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                });
            }

            // Force square corners + no shadow on the borderless window.
            // Windows 11's DWM rounds borderless windows by default and adds
            // a subtle drop-shadow; both create the visible "edge ring" we
            // want gone for the flat look.
            #[cfg(target_os = "windows")]
            {
                use tauri::Manager;
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.set_shadow(false);
                    if let Ok(hwnd) = window.hwnd() {
                        unsafe {
                            use windows::Win32::Foundation::HWND;
                            use windows::Win32::Graphics::Dwm::{
                                DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE,
                                DWMWCP_DONOTROUND,
                            };
                            let pref: i32 = DWMWCP_DONOTROUND.0;
                            let _ = DwmSetWindowAttribute(
                                HWND(hwnd.0 as *mut _),
                                DWMWA_WINDOW_CORNER_PREFERENCE,
                                &pref as *const _ as *const _,
                                std::mem::size_of_val(&pref) as u32,
                            );
                        }
                    }
                }
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .map_err(|e| anyhow::anyhow!("Error while running tauri application: {}", e))?;

    // App has fully exited. Per-pane MCP servers and their temp
    // artifacts get GC'd by the OS along with the process, but be
    // explicit about pruning so a long-running dev workstation never
    // accumulates stale dirs even if the next launch never happens.
    // Symmetric with the launch-time prune — belt-and-suspenders.
    crate::mcp_injector::prune_pane_artifacts();

    Ok(())
}
