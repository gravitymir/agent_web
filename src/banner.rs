//! Neofetch-style startup banner: the Agentron mark in ASCII beside the launch
//! mode, so the active engine is unmistakable the moment the server boots in a
//! terminal. The art lives in `agentron.txt` (30 cols wide); `█` is background
//! (rendered transparent), `▒`/`▓` are the mark (rendered in brand orange).

use crate::config::Config;
use std::io::IsTerminal;

const ART: &str = include_str!("agentron.txt");
const ORANGE: &str = "38;2;224;112;5"; // brand accent #e07005

// Tidy a path for display: drop the Windows `\\?\` verbatim prefix and collapse
// the home directory to `~` so long paths stay on one line.
fn abbrev(p: &str) -> String {
    let p = p.strip_prefix(r"\\?\").unwrap_or(p);
    if let Some(home) = dirs::home_dir() {
        let h = home.to_string_lossy();
        let h = h.strip_prefix(r"\\?\").unwrap_or(&h);
        if let Some(rest) = p.strip_prefix(h.as_ref() as &str) {
            return format!("~{rest}");
        }
    }
    p.to_string()
}

pub fn print_startup(config: &Config) {
    let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let paint = |s: &str, code: &str| {
        if color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    };

    // The headline: which engine, coloured to stand out — plus engine-specific
    // settings so the banner is a full summary of how the server launches.
    let (engine, detail, engine_code, extra): (&str, String, String, Vec<(String, String)>) =
        if config.native_engine {
            let p = crate::agent::provider::Provider::from_env();
            let flag = if p.has_key() { "" } else { "  [НЕТ КЛЮЧА]" };
            (
                "native",
                format!("{} / {}{}", p.name, p.model, flag),
                format!("1;{ORANGE}"),
                vec![
                    ("THINKING".into(), if p.thinking { "вкл".into() } else { "выкл".into() }),
                    ("MAX TOK".into(), p.max_tokens.to_string()),
                ],
            )
        } else {
            (
                "CLI",
                "Claude Code".to_string(),
                "1;38;2;90;180;250".to_string(),
                vec![
                    ("PERMISSION".into(), config.permission_mode.clone()),
                    ("CLI BIN".into(), config.claude_bin.clone()),
                ],
            )
        };

    let config_dir = config.projects_root.parent().unwrap_or(&config.projects_root);
    let ver = env!("CARGO_PKG_VERSION");

    // Right column: (label, value). Empty label => a plain heading line.
    let mut rows: Vec<(String, String)> = vec![
        (String::new(), paint(&format!("Agent Web  v{ver}"), &format!("1;{ORANGE}"))),
        (String::new(), paint("──────────────────────────", &format!("2;{ORANGE}"))),
        (
            "ENGINE".into(),
            format!("{} {}  {}", paint("▶", &engine_code), paint(engine, &engine_code), detail),
        ),
    ];
    rows.extend(extra); // engine-specific settings (THINKING / MAX TOK / PERMISSION)
    rows.extend([
        ("BIND".into(), format!("http://{}", config.bind_addr)),
        ("WORKSPACE".into(), abbrev(&config.workspace_abs().display().to_string())),
        ("STORAGE".into(), abbrev(&config_dir.display().to_string())),
        ("STATIC".into(), config.static_dir.clone()),
    ]);

    let art_lines: Vec<&str> = ART.lines().collect();
    // Auto-detect the logo width from the file, so agentron.txt can be swapped for
    // art of any size without touching this code.
    let art_w = art_lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    // Auto-detect the background glyph (the most common char — `@`, `█`, `●`, …) so
    // any art style renders with a transparent background, no code change needed.
    let bg = {
        let mut counts: std::collections::HashMap<char, u32> = std::collections::HashMap::new();
        for c in ART.chars().filter(|c| !c.is_whitespace()) {
            *counts.entry(c).or_insert(0) += 1;
        }
        counts.into_iter().max_by_key(|(_, n)| *n).map(|(c, _)| c)
    };
    // Vertically centre the info block against the taller logo.
    let offset = art_lines.len().saturating_sub(rows.len()) / 2;

    let mut out = String::from("\n");
    for (i, line) in art_lines.iter().enumerate() {
        // Background glyph -> transparent; the mark's glyphs stay, in orange.
        let cell: String = line
            .chars()
            .map(|c| if Some(c) == bg { ' ' } else { c })
            .collect();
        let cell = format!("{cell:<art_w$}");
        out.push_str("  ");
        out.push_str(&if color { format!("\x1b[{ORANGE}m{cell}\x1b[0m") } else { cell });
        out.push_str("  ");
        if i >= offset && i - offset < rows.len() {
            let (label, value) = &rows[i - offset];
            if label.is_empty() {
                out.push_str(value);
            } else {
                out.push_str(&paint(&format!("{label:<9}"), &format!("1;{ORANGE}")));
                out.push(' ');
                out.push_str(value);
            }
        }
        out.push('\n');
    }
    println!("{out}");
}
