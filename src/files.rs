//! Read-only file-explorer API behind the left "Файлы" drawer.
//!
//! Scope is deliberately narrow for now:
//!
//! - **Root is the workspace** (`CWI_WORKSPACE`), never the whole disk. Paths in
//!   the API are always relative to it, and every one goes through
//!   [`agent::tools::resolve`] — the same sandbox the agent's own file tools use
//!   (lexical `..` normalization + a symlink guard on existing paths).
//! - **Read-only.** No create/rename/delete/upload, and no execution. Running
//!   files is a separate, much bigger decision (it is RCE by design) and is left
//!   for a later pass.
//! - **Admin instances only.** The owner's host has `admin = true`; the
//!   disposable executor a guest talks to does not, so a guest can't walk that
//!   VM's filesystem. Guests still export their work with the existing
//!   "download workspace" button.

use std::path::Path;
use std::sync::Arc;

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};

use crate::AppState;

/// Largest file body sent to the browser. Beyond this the preview is cut and
/// flagged — the drawer is for looking at source files, not for streaming logs.
const MAX_PREVIEW_BYTES: usize = 512 * 1024;

/// How much of a file is inspected when deciding "is this binary?".
const SNIFF_BYTES: usize = 8192;

/// Entries returned for one directory. Sending more than this would only lag the
/// drawer; the count is reported so the UI can say the listing was cut.
const MAX_ENTRIES: usize = 5000;

#[derive(Deserialize)]
pub struct PathQuery {
    /// Workspace-relative path. Empty/absent means the workspace root.
    #[serde(default)]
    path: String,
}

#[derive(Serialize)]
struct Entry {
    name: String,
    /// `"dir"` or `"file"`. Symlinks report the kind of their target; a link
    /// pointing outside the workspace is still refused on the next click by
    /// `resolve`'s symlink guard.
    kind: &'static str,
    /// Bytes, `null` for directories (we don't walk them to sum sizes).
    size: Option<u64>,
    /// RFC-3339 UTC, or `null` if the platform/filesystem won't say.
    modified: Option<String>,
}

#[derive(Serialize)]
struct Listing {
    /// The directory that was listed, relative to the workspace root ("" = root).
    path: String,
    /// Parent directory, or `null` when already at the root — this is what the
    /// UI's "up" button binds to, so the sandbox edge is expressed in the data
    /// rather than re-derived in JS.
    parent: Option<String>,
    /// Display name of the workspace root (its last path component).
    root_name: String,
    entries: Vec<Entry>,
    /// True when the directory held more than `MAX_ENTRIES` children.
    truncated: bool,
}

#[derive(Serialize)]
struct FileBody {
    path: String,
    size: u64,
    /// Absent for binary files — the UI shows a placeholder instead.
    content: Option<String>,
    binary: bool,
    /// True when `content` stops short of `size`.
    truncated: bool,
}

/// Guard shared by both routes: 403 on a non-admin (guest) instance.
fn admin_only(state: &AppState) -> Result<(), (StatusCode, &'static str)> {
    if state.admin {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, "file explorer is admin-only"))
    }
}

/// Normalize a client-supplied path to workspace-relative, forward-slash form.
/// This is what goes back out in `path`/`parent`, so the UI never handles an
/// absolute host path (and never round-trips one back to us).
fn rel_display(workspace: &Path, abs: &Path) -> String {
    abs.strip_prefix(workspace)
        .unwrap_or(abs)
        .to_string_lossy()
        .replace('\\', "/")
}

fn modified_rfc3339(meta: &std::fs::Metadata) -> Option<String> {
    let t = meta.modified().ok()?;
    Some(chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339())
}

/// `GET /api/fs/list?path=<relative>` — one directory, dirs before files.
pub async fn list(
    State(state): State<Arc<AppState>>,
    Query(q): Query<PathQuery>,
) -> axum::response::Response {
    if let Err(e) = admin_only(&state) {
        return e.into_response();
    }
    let workspace = state.config.workspace_abs();
    let dir = match crate::agent::tools::resolve(&workspace, &q.path) {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    match tokio::task::spawn_blocking(move || read_dir(&workspace, &dir)).await {
        Ok(Ok(listing)) => Json(listing).into_response(),
        Ok(Err(e)) => (StatusCode::NOT_FOUND, e).into_response(),
        Err(e) => {
            tracing::warn!("fs list task panicked: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "listing failed").into_response()
        }
    }
}

fn read_dir(workspace: &Path, dir: &Path) -> Result<Listing, String> {
    let meta = std::fs::metadata(dir).map_err(|e| format!("cannot open directory: {e}"))?;
    if !meta.is_dir() {
        return Err("not a directory".to_string());
    }
    let mut entries = Vec::new();
    let mut truncated = false;
    for item in std::fs::read_dir(dir).map_err(|e| format!("cannot read directory: {e}"))? {
        let Ok(item) = item else { continue };
        if entries.len() >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        // `metadata` follows symlinks, so a link to a directory browses like one.
        // An unreadable entry (permissions, broken link) is listed as a file with
        // no size rather than dropped — seeing it is more useful than a silent gap.
        let meta = std::fs::metadata(item.path()).ok();
        let is_dir = meta.as_ref().is_some_and(std::fs::Metadata::is_dir);
        entries.push(Entry {
            name: item.file_name().to_string_lossy().to_string(),
            kind: if is_dir { "dir" } else { "file" },
            size: if is_dir {
                None
            } else {
                meta.as_ref().map(std::fs::Metadata::len)
            },
            modified: meta.as_ref().and_then(modified_rfc3339),
        });
    }
    // Directories first, then case-insensitive by name — Explorer/Finder order.
    entries.sort_by(|a, b| {
        (a.kind == "file")
            .cmp(&(b.kind == "file"))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    let path = rel_display(workspace, dir);
    // At the root there is no parent: the sandbox stops here.
    let parent = if path.is_empty() {
        None
    } else {
        Some(match path.rsplit_once('/') {
            Some((head, _)) => head.to_string(),
            None => String::new(),
        })
    };
    Ok(Listing {
        path,
        parent,
        root_name: workspace
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| workspace.to_string_lossy().to_string()),
        entries,
        truncated,
    })
}

/// `GET /api/fs/read?path=<relative>` — a text file's contents, capped.
pub async fn read(
    State(state): State<Arc<AppState>>,
    Query(q): Query<PathQuery>,
) -> axum::response::Response {
    if let Err(e) = admin_only(&state) {
        return e.into_response();
    }
    let workspace = state.config.workspace_abs();
    let file = match crate::agent::tools::resolve(&workspace, &q.path) {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    match tokio::task::spawn_blocking(move || read_file(&workspace, &file)).await {
        Ok(Ok(body)) => Json(body).into_response(),
        Ok(Err(e)) => (StatusCode::NOT_FOUND, e).into_response(),
        Err(e) => {
            tracing::warn!("fs read task panicked: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "read failed").into_response()
        }
    }
}

fn read_file(workspace: &Path, file: &Path) -> Result<FileBody, String> {
    let meta = std::fs::metadata(file).map_err(|e| format!("cannot open file: {e}"))?;
    if meta.is_dir() {
        return Err("path is a directory".to_string());
    }
    let size = meta.len();
    let path = rel_display(workspace, file);

    let bytes = read_capped(file, MAX_PREVIEW_BYTES)?;
    let truncated = (bytes.len() as u64) < size;
    if is_binary(&bytes) {
        return Ok(FileBody {
            path,
            size,
            content: None,
            binary: true,
            truncated,
        });
    }
    // A cut at MAX_PREVIEW_BYTES can land mid-character; `from_utf8_lossy` keeps
    // the preview instead of failing the whole read over a split multibyte char.
    Ok(FileBody {
        path,
        size,
        content: Some(String::from_utf8_lossy(&bytes).to_string()),
        binary: false,
        truncated,
    })
}

/// Read at most `cap` bytes. Deliberately not `fs::read`: the file may be huge
/// (or growing), and the point is to never allocate more than the preview needs.
fn read_capped(file: &Path, cap: usize) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let f = std::fs::File::open(file).map_err(|e| format!("cannot open file: {e}"))?;
    let mut buf = Vec::new();
    f.take(cap as u64)
        .read_to_end(&mut buf)
        .map_err(|e| format!("cannot read file: {e}"))?;
    Ok(buf)
}

/// A NUL byte in the first few KB is the classic "this is not text" signal (it's
/// what `git` and `grep` use); UTF-16 and most binary formats trip it immediately.
fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(SNIFF_BYTES).any(|&b| b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The explorer's sandbox is `agent::tools::resolve`; these assert the
    /// wiring, not the rule itself (that has its own tests in `tools.rs`).
    #[test]
    fn escapes_are_rejected_before_any_io() {
        let ws = Path::new("workspace");
        assert!(crate::agent::tools::resolve(ws, "../../etc/passwd").is_err());
        assert!(crate::agent::tools::resolve(ws, "sub/../../..").is_err());
        assert!(crate::agent::tools::resolve(ws, "src/main.rs").is_ok());
        assert!(crate::agent::tools::resolve(ws, "").is_ok()); // root itself
    }

    #[test]
    fn binary_sniffing() {
        assert!(!is_binary(b"fn main() {}\n"));
        assert!(!is_binary("Привет, мир\n".as_bytes()));
        assert!(is_binary(b"PK\x03\x04\x00\x00binary"));
        assert!(!is_binary(b"")); // empty file is text, not binary
    }

    #[test]
    fn listing_walks_a_real_directory() {
        let root = std::env::temp_dir().join("cwi_fs_test_listing");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("zeta")).unwrap();
        std::fs::create_dir_all(root.join("Alpha")).unwrap();
        std::fs::write(root.join("b.txt"), b"hello").unwrap();
        std::fs::write(root.join("A.txt"), b"hi").unwrap();

        let l = read_dir(&root, &root).unwrap();
        let names: Vec<&str> = l.entries.iter().map(|e| e.name.as_str()).collect();
        // Dirs first, then files; each group case-insensitively sorted.
        assert_eq!(names, vec!["Alpha", "zeta", "A.txt", "b.txt"]);
        assert_eq!(l.entries[0].kind, "dir");
        assert_eq!(l.entries[0].size, None);
        assert_eq!(l.entries[3].size, Some(5));
        assert_eq!(l.path, "");
        assert_eq!(l.parent, None, "the root must not offer a way up");

        // One level down: parent points back at the root.
        let sub = read_dir(&root, &root.join("Alpha")).unwrap();
        assert_eq!(sub.path, "Alpha");
        assert_eq!(sub.parent.as_deref(), Some(""));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reading_caps_and_flags_truncation() {
        let root = std::env::temp_dir().join("cwi_fs_test_read");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let small = root.join("small.txt");
        std::fs::write(&small, "привет").unwrap();
        let body = read_file(&root, &small).unwrap();
        assert_eq!(body.content.as_deref(), Some("привет"));
        assert!(!body.truncated && !body.binary);

        let big = root.join("big.bin");
        std::fs::write(&big, vec![b'x'; MAX_PREVIEW_BYTES + 100]).unwrap();
        let body = read_file(&root, &big).unwrap();
        assert!(body.truncated, "an oversized file must report truncation");
        assert_eq!(body.content.map(|c| c.len()), Some(MAX_PREVIEW_BYTES));
        assert_eq!(body.size, (MAX_PREVIEW_BYTES + 100) as u64);

        let bin = root.join("logo.png");
        std::fs::write(&bin, b"\x89PNG\r\n\x1a\n\x00\x00").unwrap();
        let body = read_file(&root, &bin).unwrap();
        assert!(body.binary && body.content.is_none());

        assert!(
            read_file(&root, &root).is_err(),
            "a directory is not a file"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
