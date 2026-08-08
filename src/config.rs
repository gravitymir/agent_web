use std::path::PathBuf;

/// Runtime configuration, read from environment variables with sensible
/// defaults. All variables are optional.
#[derive(Clone, Debug)]
pub struct Config {
    /// Address the HTTP/WebSocket server binds to, e.g. `127.0.0.1:8787`.
    pub bind_addr: String,
    /// Working directory Claude Code runs in. Sessions are stored per this
    /// directory, so it defines which chats are visible. Defaults to the
    /// server's current working directory.
    pub workspace_dir: PathBuf,
    /// The `claude` executable to invoke. Defaults to `claude` on PATH.
    pub claude_bin: String,
    /// Permission mode passed to Claude Code. `acceptEdits`/`bypassPermissions`
    /// would auto-approve at the CLI level *before* a decision ever reaches our
    /// `--permission-prompt-tool stdio` handler (`run_actor`'s `control_request`
    /// handling in session.rs) — silently defeating the caps panel's "Изменение
    /// файлов" toggle for Write/Edit specifically. `default` routes every
    /// tool that needs a decision through that handler instead, so the caps
    /// panel actually gates it. `Read`/`Glob`/`Grep` are never gated by Claude
    /// Code at any permission mode — that's unrelated to this setting.
    pub permission_mode: String,
    /// Root of Claude Code's on-disk sessions (`<claude-config-dir>/projects`).
    /// The config dir honors `CLAUDE_CONFIG_DIR`, so the web app can run fully
    /// isolated from the user's desktop/terminal Claude (see [`claude_config_dir`]).
    pub projects_root: PathBuf,
    /// Use the native `/v1/messages` agent engine instead of the Claude Code CLI.
    /// Enabled with `CWI_ENGINE=native`.
    pub native_engine: bool,
    /// Directory served for the frontend (`CWI_STATIC_DIR`, default `static`).
    pub static_dir: String,
}

/// Locate the frontend `static/` directory robustly, so the app serves the web
/// UI no matter how it's launched (from the project root via `cargo run`, or by
/// running the built exe from any working directory / a double-click).
///
/// Order: an explicit `CWI_STATIC_DIR` always wins; otherwise try the cwd, then
/// next to the executable (bundled layout), then two levels up from the exe
/// (`target/<profile>/agent_web.exe` → project root). The first candidate that
/// actually contains `index.html` is used; fall back to `"static"`.
fn resolve_static_dir() -> String {
    if let Ok(s) = std::env::var("CWI_STATIC_DIR") {
        return s;
    }
    let mut candidates: Vec<PathBuf> = vec![PathBuf::from("static")];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("static"));
            candidates.push(dir.join("..").join("..").join("static"));
        }
    }
    for c in &candidates {
        if c.join("index.html").is_file() {
            // Canonicalize so the banner shows a clean absolute path (no `..\..`);
            // fall back to the raw path if canonicalization fails. The `\\?\`
            // verbatim prefix Windows adds is stripped later for display.
            let path = std::fs::canonicalize(c).unwrap_or_else(|_| c.clone());
            return path.to_string_lossy().into_owned();
        }
    }
    "static".to_string()
}

impl Config {
    pub fn from_env() -> Self {
        let bind_addr =
            std::env::var("CWI_BIND").unwrap_or_else(|_| "127.0.0.1:8787".to_string());

        let workspace_dir = std::env::var("CWI_WORKSPACE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
            });

        let claude_bin = std::env::var("CWI_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string());

        let permission_mode =
            std::env::var("CWI_PERMISSION_MODE").unwrap_or_else(|_| "default".to_string());

        let projects_root = claude_config_dir().join("projects");

        let native_engine = std::env::var("CWI_ENGINE")
            .map(|v| v.eq_ignore_ascii_case("native"))
            .unwrap_or(false);

        let static_dir = resolve_static_dir();

        Self {
            bind_addr,
            workspace_dir,
            claude_bin,
            permission_mode,
            projects_root,
            native_engine,
            static_dir,
        }
    }

    /// Absolute, normalized workspace path used both to run Claude and to
    /// locate its session directory.
    pub fn workspace_abs(&self) -> PathBuf {
        match std::fs::canonicalize(&self.workspace_dir) {
            Ok(p) => p,
            Err(e) => {
                // Falling back to the raw path silently can point `session_dir()`
                // at the wrong folder. Warn once (this is called per request, so
                // don't spam) and carry on with the non-canonical path.
                static WARN_ONCE: std::sync::Once = std::sync::Once::new();
                WARN_ONCE.call_once(|| {
                    tracing::warn!(
                        "canonicalize({}) failed: {e}; using the path as-is",
                        self.workspace_dir.display()
                    );
                });
                self.workspace_dir.clone()
            }
        }
    }

    /// The `~/.claude/projects/<encoded>` directory holding this workspace's
    /// session `.jsonl` files.
    pub fn session_dir(&self) -> PathBuf {
        self.projects_root.join(encode_project_dir(&self.workspace_abs()))
    }
}

/// The Claude Code config/state directory, holding sessions, credentials, and
/// this app's own metadata. Honors `CLAUDE_CONFIG_DIR` — the same variable the
/// `claude` CLI itself reads — so pointing the web app (and the `claude`
/// subprocess it spawns, which inherits the env) at a dedicated directory gives
/// it its own chats and history, never mixing with the user's desktop/terminal
/// Claude. Falls back to `~/.claude`.
pub fn claude_config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
}

/// Claude Code encodes a project's absolute path into a directory name by
/// replacing every non-alphanumeric character with `-`.
///
/// e.g. `C:\Users\gravi\Documents\rust\agent_web`
///   -> `C--Users-gravi-Documents-rust-agent-web`
pub fn encode_project_dir(path: &std::path::Path) -> String {
    let s = path.to_string_lossy();
    // Strip the Windows verbatim prefix `\\?\` that canonicalize adds.
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::encode_project_dir;
    use std::path::Path;

    #[test]
    fn encodes_non_alnum_to_dash() {
        assert_eq!(
            encode_project_dir(Path::new(r"C:\Users\gravi\agent_web")),
            "C--Users-gravi-agent-web"
        );
        assert_eq!(encode_project_dir(Path::new("/home/u/my proj")), "-home-u-my-proj");
    }

    #[test]
    fn strips_verbatim_prefix() {
        assert_eq!(encode_project_dir(Path::new(r"\\?\C:\a")), "C--a");
    }
}
