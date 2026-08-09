//! Interactive startup menu: pick the engine and port with arrow keys before the
//! server boots. Runs only on an interactive terminal — piped/service/headless
//! launches (and `CWI_NO_MENU=1`) skip it and fall back to env/`.env`/defaults.
//!
//! Choices are applied by setting the same env vars `Config::from_env` already
//! reads (`CWI_ENGINE`, `CWI_AGENT_PROVIDER`, `CWI_BIND`), so there is a single
//! source of truth and the spawned `claude`/provider inherit them.

use dialoguer::{theme::ColorfulTheme, Select};
use std::io::IsTerminal;

// (label, provider). `None` => Cloud subscription via the Claude Code CLI.
const ENGINES: &[(&str, Option<&str>)] = &[
    ("Cloud — подписка Claude Code CLI", None),
    ("Native — Anthropic API", Some("anthropic")),
    ("Native — Kimi (Moonshot)", Some("kimi")),
    ("Native — GLM (Z.ai)", Some("glm")),
    ("Native — Qwen (Alibaba)", Some("qwen")),
    ("Native — Gemini (Google)", Some("gemini")),
];
const PORTS: &[u16] = &[8787, 8788, 8789, 8790, 8791];

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
    let engine_default = if !cur_native {
        0
    } else {
        ENGINES
            .iter()
            .position(|(_, p)| *p == Some(cur_provider.as_str()))
            .unwrap_or(1)
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
    match ENGINES[engine_idx].1 {
        None => unsafe { std::env::set_var("CWI_ENGINE", "cli") },
        Some(provider) => unsafe {
            std::env::set_var("CWI_ENGINE", "native");
            std::env::set_var("CWI_AGENT_PROVIDER", provider);
        },
    }

    // --- Port -----------------------------------------------------------------
    let cur_port = std::env::var("CWI_BIND")
        .ok()
        .and_then(|b| b.rsplit(':').next().and_then(|p| p.parse::<u16>().ok()));
    let port_default = cur_port
        .and_then(|p| PORTS.iter().position(|x| *x == p))
        .unwrap_or(0);
    let port_labels: Vec<String> = PORTS.iter().map(|p| p.to_string()).collect();
    let port_idx = Select::with_theme(&theme)
        .with_prompt("Порт")
        .items(&port_labels)
        .default(port_default)
        .interact()
        .unwrap_or(port_default);

    // SAFETY: as above — synchronous startup, no other threads yet.
    unsafe { std::env::set_var("CWI_BIND", format!("127.0.0.1:{}", PORTS[port_idx])) };
}
