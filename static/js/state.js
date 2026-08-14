"use strict";

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------
export const state = {
  ws: null,
  sessionId: null,   // current chat's session id (null = nothing open yet)
  isNew: true,       // true until the current chat's first turn is sent
  streaming: false,  // true while Claude is producing a turn
  current: null,     // active assistant render context
  pendingImages: [], // pasted images awaiting send: {media_type, data, url}
  pendingFiles: [],  // attached text files awaiting send: {name, text, truncated}
  chatUsage: {},     // session_id -> {tokens,input_tokens,cache_read,cache_creation,turns,duration_ms,contextTokens}
  providers: [],     // native engine: [{id,name,has_key,models}]
  activeProvider: "", // native engine: the server's configured provider (wizard/env)
  activeModel: "",    // native engine: the server's configured model (preset/CWI_AGENT_MODEL)
  replayMode: false, // true while replaying a live session's scrollback
  transcript: null,  // {msgs,start} loaded from disk for the current chat
  engineNative: null, // active engine: true=native, false=cli, null=unknown yet
  chatEngine: {},     // session_id -> "cli" | "native" (which store owns the chat)
  usage: null,        // subscription limits from /api/usage (or null if N/A)
  followBottom: true, // true while the user is reading the tail (near the bottom);
                      // streamed output pulls the view down only while this holds
};

// A chat is "frozen" (read-only) when its owning engine differs from the active
// one — you can read/scroll it, but not send a turn until CWI_ENGINE is switched.
// Unknown engine (brand-new chat, or before /api/providers resolves) → not frozen.
export function chatFrozen(id) {
  if (id == null || state.engineNative == null) return false;
  const owner = state.chatEngine[id];
  if (!owner) return false;
  return owner !== (state.engineNative ? "native" : "cli");
}

export const el = {
  chatList: document.getElementById("chat-list"),
  chatSearch: document.getElementById("chat-search"),
  messages: document.getElementById("messages"),
  scrollToBottomBtn: document.getElementById("scroll-to-bottom"),
  scrollbar: document.getElementById("scrollbar"),
  scrollbarThumb: document.getElementById("scrollbar-thumb"),
  input: document.getElementById("input"),
  composer: document.getElementById("composer"),
  composerRow: document.querySelector("#composer .composer-row"),
  frozenBanner: document.getElementById("frozen-banner"),
  send: document.getElementById("send"),
  stop: document.getElementById("stop"),
  mic: document.getElementById("mic"),
  attachBtn: document.getElementById("attach-btn"),
  fileInput: document.getElementById("file-input"),
  toolsBtn: document.getElementById("tools-btn"),
  toolsModal: document.getElementById("tools-modal"),
  toolsClose: document.getElementById("tools-close"),
  newChat: document.getElementById("new-chat"),
  bigNewChat: document.getElementById("big-new-chat"),
  attachPreview: document.getElementById("attach-preview"),
  title: document.getElementById("chat-title"),
  chatControls: document.getElementById("chat-controls"),
  model: document.getElementById("model-select"),
  provider: document.getElementById("provider-select"),
  providerSection: document.getElementById("provider-section"),
  offlineBanner: document.getElementById("offline-banner"),
  offlineDetail: document.getElementById("offline-detail"),
  // chat list drawer
  sidebar: document.getElementById("sidebar"),
  sidebarBadge: document.getElementById("sidebar-badge"),
  sidebarOverlay: document.getElementById("sidebar-overlay"),
  // admin controls drawer (Гостевой сервер + Ссылки) — owner instance only
  adminDrawer: document.getElementById("admin-drawer"),
  adminBadge: document.getElementById("admin-badge"),
  adminOverlay: document.getElementById("admin-overlay"),
  usageBadge: document.getElementById("usage-badge"),
  usagePanel: document.getElementById("usage-panel"),
  usageOverlay: document.getElementById("usage-overlay"),
  usageDetail: document.getElementById("usage-detail"),
  // settings
  settingsBadge: document.getElementById("settings-badge"),
  settingsPanel: document.getElementById("settings-panel"),
  usageInfo: document.getElementById("usage-info"),
  settingsOverlay: document.getElementById("settings-overlay"),
  fontSeg: document.getElementById("fontsize-seg"),
  autosend: document.getElementById("autosend-check"),
  sound: document.getElementById("sound-check"),
  notify: document.getElementById("notify-check"),
  // Context-management settings (direct-API / Gemini engine only)
  ctxSection: document.getElementById("ctx-section"),
  ctxMgmt: document.getElementById("ctx-mgmt-check"),
  ctxControls: document.getElementById("ctx-controls"),
  ctxNudge: document.getElementById("ctx-nudge"),
  ctxNudgeVal: document.getElementById("ctx-nudge-val"),
  ctxCompress: document.getElementById("ctx-compress"),
  ctxCompressVal: document.getElementById("ctx-compress-val"),
  ctxAuto: document.getElementById("ctx-auto"),
  ctxMore: document.getElementById("ctx-more"),
  // modal
  modalOverlay: document.getElementById("modal-overlay"),
  iconGrid: document.getElementById("icon-grid"),
  chatName: document.getElementById("chat-name"),
  modalCreate: document.getElementById("modal-create"),
  modalCancel: document.getElementById("modal-cancel"),
  actionModalOverlay: document.getElementById("action-modal-overlay"),
  actionModalTitle: document.getElementById("action-modal-title"),
  actionModalMessage: document.getElementById("action-modal-message"),
  actionModalConfirm: document.getElementById("action-modal-confirm"),
  actionModalCancel: document.getElementById("action-modal-cancel"),
  faviconLink: document.getElementById("favicon-link"),
};

// Icon palette for new chats — iOS-style monochrome SVG keys (see ios-icons.js).
export const ICONS = [
  "stopwatch",
  "document","chat","bug","rocket","pencil","gear","brain","chart","search","lightbulb",
  "folder","globe","flask","lock","archive","pin","target","server","puzzle","book",
  "check","image","film","music","robot","bubble","microscope","calculator","calendar","star",
];

// ---------------------------------------------------------------------------
// Minimal Markdown renderer (self-contained, HTML-escaping)
// ---------------------------------------------------------------------------
export function escapeHtml(s) {
  return String(s)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

// ---------------------------------------------------------------------------
// LaTeX / math rendering helpers
// ---------------------------------------------------------------------------
const MATH_PLACEHOLDER = "\uE000MATH";
const mathBlocks = []; // accumulated during each renderMarkdown call

function extractMath(src) {
  const blocks = [];
  // Block math: $$...$$ or \[...\]
  src = src.replace(
    /(\$\$[\s\S]*?\$\$|\\\[[\s\S]*?\\\])/g,
    (m) => {
      const idx = blocks.length;
      blocks.push(m);
      return `${MATH_PLACEHOLDER}${idx}\uE000`;
    }
  );
  // Inline math: \(...\) or $...$ (single dollar, not double)
  src = src.replace(
    /(\\\([\s\S]*?\\\)|\$[^$\n]+?\$)/g,
    (m) => {
      const idx = blocks.length;
      blocks.push(m);
      return `${MATH_PLACEHOLDER}${idx}\uE000`;
    }
  );
  return { src, blocks };
}

function renderMath(src) {
  if (typeof window === "undefined" || !window.katex) return null;
  try {
    const display = src.startsWith("$$") || src.startsWith("\\[");
    return window.katex.renderToString(src.replace(/^\$\$|\\\[|\\\(|\$|\$\$|\\\]$/g, ""), {
      throwOnError: false,
      displayMode: display,
    });
  } catch (e) {
    return `<span class="math-error" title="${escapeHtml(String(e))}">${escapeHtml(src)}</span>`;
  }
}

// --- GFM tables -------------------------------------------------------------
// Split a table row into trimmed cells, tolerating optional leading/trailing `|`.
export function tableCells(line) {
  let s = line.trim();
  if (s.startsWith("|")) s = s.slice(1);
  if (s.endsWith("|")) s = s.slice(0, -1);
  return s.split("|").map((c) => c.trim());
}
// A separator row like |---|:--:|--:| — every cell is dashes with optional colons.
export function isTableSep(line) {
  if (!line.includes("|") && !line.includes("-")) return false;
  const cells = tableCells(line);
  return cells.length > 0 && cells.every((c) => /^:?-+:?$/.test(c));
}
export function cellAlign(sep) {
  const l = sep.startsWith(":"), r = sep.endsWith(":");
  if (l && r) return "center";
  if (r) return "right";
  if (l) return "left";
  return "";
}
export function renderTable(headers, aligns, rows) {
  const al = (i) => (aligns[i] ? ` style="text-align:${aligns[i]}"` : "");
  const th = headers.map((c, i) => `<th${al(i)}>${c}</th>`).join("");
  const body = rows
    .map((cells) => {
      const tds = headers
        .map((_, i) => `<td${al(i)}>${cells[i] != null ? cells[i] : ""}</td>`)
        .join("");
      return `<tr>${tds}</tr>`;
    })
    .join("");
  return `<table><thead><tr>${th}</tr></thead><tbody>${body}</tbody></table>`;
}

export function renderMarkdown(src) {
  mathBlocks.length = 0;
  const mathEx = extractMath(src);
  src = mathEx.src;
  mathBlocks.push(...mathEx.blocks);

  const codeBlocks = [];
  // Fenced code blocks (support unterminated fence while streaming).
  src = src.replace(/```([^\n`]*)\n([\s\S]*?)(?:```|$)/g, (m, lang, code) => {
    const idx = codeBlocks.length;
    codeBlocks.push({ lang: lang.trim(), code });
    return `\uE000CODE${idx}\uE000`;
  });

  let html = escapeHtml(src);

  // Inline code
  html = html.replace(/`([^`\n]+)`/g, (m, c) => `<code class="inline">${c}</code>`);

  // Bold / italic
  html = html.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
  html = html.replace(/(^|[^*])\*([^*\n]+)\*/g, "$1<em>$2</em>");

  // Links [text](url) — only http/https are linkified (so `javascript:` etc.
  // never become anchors). `escapeHtml(src)` above already neutralized <>&"' in
  // the whole string, so the captured URL can't break out of the href attribute.
  html = html.replace(/\[([^\]]+)\]\((https?:\/\/[^\s)]+)\)/g, (m, text, url) => {
    return `<a href="${url}" target="_blank" rel="noopener">${text}</a>`;
  });

  // Block-level: headings, lists, paragraphs
  const lines = html.split("\n");
  const out = [];
  let listType = null; // "ul" | "ol"

  const closeList = () => {
    if (listType) { out.push(`</${listType}>`); listType = null; }
  };

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i].trimEnd();

    if (/^\uE000CODE\d+\uE000$/.test(line.trim())) {
      closeList();
      out.push(line.trim());
      continue;
    }

    const mathMatch = line.trim().match(/^\uE000MATH(\d+)\uE000$/);
    if (mathMatch) {
      const rendered = renderMath(mathBlocks[+mathMatch[1]]);
      if (rendered) { closeList(); out.push(rendered); continue; }
    }

    // GFM table: a header row immediately followed by a |---|---| separator.
    if (line.includes("|") && i + 1 < lines.length && isTableSep(lines[i + 1])) {
      closeList();
      const headers = tableCells(line);
      const aligns = tableCells(lines[i + 1]).map(cellAlign);
      const rows = [];
      let j = i + 2;
      while (j < lines.length) {
        const r = lines[j].trim();
        if (r === "" || !r.includes("|") || /^CODE\d+$/.test(r)) break;
        rows.push(tableCells(lines[j]));
        j++;
      }
      out.push(renderTable(headers, aligns, rows));
      i = j - 1;
      continue;
    }
    const h = line.match(/^(#{1,3})\s+(.*)$/);
    if (h) {
      closeList();
      const lvl = h[1].length;
      out.push(`<h${lvl}>${h[2]}</h${lvl}>`);
      continue;
    }
    const ul = line.match(/^\s*[-*]\s+(.*)$/);
    if (ul) {
      if (listType !== "ul") { closeList(); out.push("<ul>"); listType = "ul"; }
      out.push(`<li>${ul[1]}</li>`);
      continue;
    }
    const ol = line.match(/^\s*\d+\.\s+(.*)$/);
    if (ol) {
      if (listType !== "ol") { closeList(); out.push("<ol>"); listType = "ol"; }
      out.push(`<li>${ol[1]}</li>`);
      continue;
    }
    if (line.trim() === "") {
      closeList();
      out.push("");
      continue;
    }
    closeList();
    out.push(`<p>${line}</p>`);
  }
  closeList();
  html = out.join("\n");

  // Reinsert code blocks with copy buttons.
  html = html.replace(/\uE000CODE(\d+)\uE000/g, (m, i) => {
    const { lang, code } = codeBlocks[+i];
    const clean = code.replace(/\n$/, "");
    const hi = highlightCode(escapeHtml(clean), lang);
    const langLabel = lang ? ` data-lang="${escapeHtml(lang)}"` : "";
    const preview = isHtmlLang(lang, clean)
      ? '<button class="copy-btn preview-btn" onclick="previewCode(this)">предпросмотр</button>'
      : "";
    return `<pre${langLabel}>${preview}<button class="copy-btn" onclick="copyCode(this)">копировать</button><code>${hi}</code></pre>`;
  });

  return html;
}
window.copyCode = function (btn) {
  const code = btn.parentElement.querySelector("code");
  navigator.clipboard.writeText(code.textContent).then(() => {
    const old = btn.textContent;
    btn.textContent = "скопировано";
    setTimeout(() => (btn.textContent = old), 1200);
  });
};

// --- Lightweight syntax highlighting ---------------------------------------
// Operates on ALREADY HTML-escaped source (so `<`,`>`,`&` are entities). A
// single scanning regex tokenizes comments / strings / literals / keywords /
// numbers; gaps pass through untouched, so entities are never corrupted.
// Deliberately language-agnostic — a broad keyword union covers the common
// languages well enough without per-grammar machinery (this is a chat view).
const HL_KEYWORDS =
  "const|let|var|function|fn|def|class|struct|enum|impl|trait|pub|use|mod|" +
  "return|if|else|elif|for|while|loop|match|switch|case|break|continue|in|of|" +
  "new|this|self|super|async|await|yield|import|from|export|default|try|catch|" +
  "except|finally|throw|raise|with|as|is|not|and|or|type|interface|extends|" +
  "implements|public|private|protected|static|void|package|func|go|defer|chan|" +
  "map|range|where|do|then|end|module|require|include|namespace|template|" +
  "override|virtual|final|abstract|lambda|global|nonlocal|pass|del|assert|" +
  "mut|dyn|move|ref|unsafe|extern|crate|print|println|echo";
const HL_LITERALS = "true|false|null|nil|None|True|False|undefined";
const HL_HASH_LANGS = new Set([
  "py", "python", "bash", "sh", "shell", "zsh", "ruby", "rb", "yaml", "yml",
  "toml", "ini", "r", "perl", "makefile", "make", "dockerfile", "conf",
]);

export function highlightCode(escaped, lang) {
  const l = (lang || "").toLowerCase();
  const hash = HL_HASH_LANGS.has(l) ? "|#[^\\n]*" : "";
  const re = new RegExp(
    "(\\/\\/[^\\n]*|\\/\\*[\\s\\S]*?\\*\\/" + hash + ")" + // 1 comment
      "|(\"(?:[^\"\\\\]|\\\\.)*\"|'(?:[^'\\\\]|\\\\.)*'|`(?:[^`\\\\]|\\\\.)*`)" + // 2 string
      "|(\\b(?:" + HL_LITERALS + ")\\b)" + // 3 literal
      "|(\\b(?:" + HL_KEYWORDS + ")\\b)" + // 4 keyword
      "|(\\b0x[0-9a-fA-F]+\\b|\\b\\d+(?:\\.\\d+)?\\b)", // 5 number
    "g"
  );
  let out = "", last = 0, m;
  while ((m = re.exec(escaped)) !== null) {
    if (m[0].length === 0) { re.lastIndex++; continue; } // zero-width guard
    if (m.index > last) out += escaped.slice(last, m.index);
    const cls = m[1] ? "c" : m[2] ? "s" : m[3] ? "l" : m[4] ? "k" : "n";
    out += `<span class="tok-${cls}">${m[0]}</span>`;
    last = m.index + m[0].length;
  }
  out += escaped.slice(last);
  return out;
}

// Heuristic: is this code block renderable HTML? An explicit lang wins;
// otherwise sniff for a leading tag / doctype.
function isHtmlLang(lang, code) {
  const l = (lang || "").toLowerCase();
  if (l === "html" || l === "svg" || l === "xml") return true;
  if (l) return false; // a different language was declared
  return /^\s*<(!doctype html|html|svg|div|section|body|head|table|ul|ol|p|h[1-6])[\s>]/i.test(code);
}

// Open a code block's HTML in a sandboxed iframe overlay (no allow-same-origin,
// so it can't touch the app). Code is read from the sibling <code> element.
window.previewCode = function (btn) {
  const code = btn.parentElement.querySelector("code").textContent;
  const overlay = document.createElement("div");
  overlay.className = "preview-overlay";
  const frame = document.createElement("iframe");
  frame.className = "preview-frame";
  frame.setAttribute("sandbox", "allow-scripts");
  frame.srcdoc = code;
  const close = document.createElement("button");
  close.className = "preview-close";
  close.innerHTML = `<svg class="ios-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><line x1="18" y1="6" x2="6" y2="18" /><line x1="6" y1="6" x2="18" y2="18" /></svg>`;
  close.title = "Закрыть предпросмотр";

  const shut = () => { overlay.remove(); document.removeEventListener("keydown", esc); };
  function esc(e) { if (e.key === "Escape") shut(); }
  close.addEventListener("click", shut);
  overlay.addEventListener("click", (e) => { if (e.target === overlay) shut(); });
  document.addEventListener("keydown", esc);
  overlay.appendChild(close);
  overlay.appendChild(frame);
  document.body.appendChild(overlay);
};
