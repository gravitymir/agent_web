//! Interactive startup menu: pick the engine (Cloud subscription vs a Native API
//! provider) with arrow keys before the server boots. Runs only on an interactive
//! terminal — piped/service/headless launches (and `CWI_NO_MENU=1`) skip it and
//! fall back to env/`.env`/defaults.
//!
//! Choices are applied by setting the same env vars `Config::from_env` already
//! reads (`CWI_ENGINE`, `CWI_AGENT_PROVIDER`), so there is a single source of
//! truth and the spawned `claude`/provider inherit them. The bind port is not
//! prompted — `Config` already defaults `CWI_BIND` to `127.0.0.1:8787`.

use dialoguer::{Select, theme::ColorfulTheme};
use std::io::IsTerminal;

/// One wizard choice: which engine (and how) to launch.
enum Choice {
    /// Claude Code CLI (Cloud subscription).
    Cli,
    /// Qwen Code CLI on the ModelStudio Token Plan (key in .env:
    /// BAILIAN_TOKEN_PLAN_API_KEY); sets CWI_CLAUDE_BIN to the qwen binary.
    CliQwen,
    /// Built-in native engine with this API provider.
    Native(&'static str),
}

const ENGINES: &[(&str, Choice)] = &[
    ("Cloud — подписка Claude Code CLI", Choice::Cli),
    ("Qwen Code CLI — ModelStudio Token Plan", Choice::CliQwen),
    ("Native — Anthropic API", Choice::Native("anthropic")),
    ("Native — Kimi (Moonshot)", Choice::Native("kimi")),
    ("Native — GLM (Z.ai)", Choice::Native("glm")),
    ("Native — Qwen (Alibaba)", Choice::Native("qwen")),
    ("Native — Gemini (Google)", Choice::Native("gemini")),
];

/// The qwen binary to launch: CWI_QWEN_BIN wins, then the standalone installer's
/// well-known path, then plain "qwen" from PATH.
fn qwen_bin() -> String {
    if let Ok(b) = std::env::var("CWI_QWEN_BIN")
        && !b.trim().is_empty()
    {
        return b.trim().to_string();
    }
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        let p = std::path::Path::new(&home).join("AppData/Local/qwen-code/bin/qwen.cmd");
        if p.exists() {
            return p.to_string_lossy().into_owned();
        }
    }
    "qwen".to_string()
}

pub fn run() {
    // Only prompt when we can actually read arrow keys back.
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return;
    }
    if std::env::var_os("CWI_NO_MENU").is_some() {
        return;
    }

    let theme = ColorfulTheme::default();

    // --- Engine ---------------------------------------------------------------
    // Default the arrow to whatever env/.env already selected.
    let cur_native = std::env::var("CWI_ENGINE")
        .map(|v| v.eq_ignore_ascii_case("native"))
        .unwrap_or(false);
    let cur_provider = std::env::var("CWI_AGENT_PROVIDER").unwrap_or_default();
    let cur_qwen_cli = std::env::var("CWI_CLAUDE_BIN")
        .map(|b| b.to_ascii_lowercase().contains("qwen"))
        .unwrap_or(false);
    let engine_default = if !cur_native {
        if cur_qwen_cli { 1 } else { 0 }
    } else {
        ENGINES
            .iter()
            .position(|(_, c)| matches!(c, Choice::Native(p) if *p == cur_provider.as_str()))
            .unwrap_or(2)
    };
    let labels: Vec<&str> = ENGINES.iter().map(|(l, _)| *l).collect();
    let engine_idx = Select::with_theme(&theme)
        .with_prompt("Движок запуска")
        .items(&labels)
        .default(engine_default)
        .interact()
        .unwrap_or(engine_default);

    // SAFETY: `run()` is called only from `main`'s synchronous startup phase,
    // before the async runtime/threads exist — no concurrent environment access.
    match &ENGINES[engine_idx].1 {
        Choice::Cli => unsafe {
            std::env::set_var("CWI_ENGINE", "cli");
            // A leftover qwen path (e.g. from .env) would silently flip the CLI
            // flavor; Cloud must mean the claude binary.
            if std::env::var("CWI_CLAUDE_BIN")
                .map(|b| b.to_ascii_lowercase().contains("qwen"))
                .unwrap_or(false)
            {
                std::env::remove_var("CWI_CLAUDE_BIN");
            }
        },
        Choice::CliQwen => unsafe {
            std::env::set_var("CWI_ENGINE", "cli");
            std::env::set_var("CWI_CLAUDE_BIN", qwen_bin());
        },
        Choice::Native(provider) => unsafe {
            std::env::set_var("CWI_ENGINE", "native");
            std::env::set_var("CWI_AGENT_PROVIDER", provider);
        },
    }
    // No port prompt: the server binds `CWI_BIND` (Config defaults it to
    // 127.0.0.1:8787). Set CWI_BIND in the environment/.env to override.
}
