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
    /// Permission mode passed to Claude Code. In non-interactive (`--print`)
    /// mode there is no human to answer prompts, so a non-blocking mode is
    /// required for tools to run. Defaults to `acceptEdits`.
    pub permission_mode: String,
    /// Root of Claude Code's on-disk sessions (`~/.claude/projects`).
    pub projects_root: PathBuf,
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
            std::env::var("CWI_PERMISSION_MODE").unwrap_or_else(|_| "acceptEdits".to_string());

        let projects_root = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".claude")
            .join("projects");

        Self {
            bind_addr,
            workspace_dir,
            claude_bin,
            permission_mode,
            projects_root,
        }
    }

    /// Absolute, normalized workspace path used both to run Claude and to
    /// locate its session directory.
    pub fn workspace_abs(&self) -> PathBuf {
        std::fs::canonicalize(&self.workspace_dir)
            .unwrap_or_else(|_| self.workspace_dir.clone())
    }

    /// The `~/.claude/projects/<encoded>` directory holding this workspace's
    /// session `.jsonl` files.
    pub fn session_dir(&self) -> PathBuf {
        self.projects_root.join(encode_project_dir(&self.workspace_abs()))
    }
}

/// Claude Code encodes a project's absolute path into a directory name by
/// replacing every non-alphanumeric character with `-`.
///
/// e.g. `C:\Users\gravi\Documents\rust\claude_web_interface`
///   -> `C--Users-gravi-Documents-rust-claude-web-interface`
pub fn encode_project_dir(path: &std::path::Path) -> String {
    let s = path.to_string_lossy();
    // Strip the Windows verbatim prefix `\\?\` that canonicalize adds.
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}
