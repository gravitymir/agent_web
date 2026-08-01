"use strict";

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------
const state = {
  ws: null,
  sessionId: null,   // current chat's session id (null = nothing open yet)
  isNew: true,       // true until the current chat's first turn is sent
  streaming: false,  // true while Claude is producing a turn
  current: null,     // active assistant render context
};

const el = {
  chatList: document.getElementById("chat-list"),
  messages: document.getElementById("messages"),
  input: document.getElementById("input"),
  composer: document.getElementById("composer"),
  send: document.getElementById("send"),
  stop: document.getElementById("stop"),
  newChat: document.getElementById("new-chat"),
  bigNewChat: document.getElementById("big-new-chat"),
  title: document.getElementById("chat-title"),
  model: document.getElementById("model-select"),
  offlineBanner: document.getElementById("offline-banner"),
  // chat list drawer
  sidebar: document.getElementById("sidebar"),
  sidebarBadge: document.getElementById("sidebar-badge"),
  sidebarOverlay: document.getElementById("sidebar-overlay"),
  // settings
  settingsBadge: document.getElementById("settings-badge"),
  settingsPanel: document.getElementById("settings-panel"),
  settingsOverlay: document.getElementById("settings-overlay"),
  themeSeg: document.getElementById("theme-seg"),
  fontSeg: document.getElementById("fontsize-seg"),
  // modal
  modalOverlay: document.getElementById("modal-overlay"),
  iconGrid: document.getElementById("icon-grid"),
  chatName: document.getElementById("chat-name"),
  modalCreate: document.getElementById("modal-create"),
  modalCancel: document.getElementById("modal-cancel"),
};

// Icon palette for new chats.
const ICONS = [
  "📁","💻","🐛","🚀","📝","🎨","🔧","🧠","📊","🔍",
  "💡","🗂️","🌐","🧪","🔐","📦","⚙️","📌","🎯","🗄️",
  "🧩","📖","✅","🖼️","🎬","🎵","🤖","💬","🔬","🧮",
];

// ---------------------------------------------------------------------------
// WebSocket
// ---------------------------------------------------------------------------
function connect() {
  const proto = location.protocol === "https:" ? "wss" : "ws";
  const ws = new WebSocket(`${proto}://${location.host}/ws`);
  state.ws = ws;

  ws.onopen = () => {
    setConn(true);
    // Reconnect: re-attach to the live session (if any) to resume its stream.
    if (state.sessionId && !state.isNew) {
      sendWs({ type: "attach", session_id: state.sessionId });
    }
  };
  ws.onclose = () => {
    setConn(false);
    setTimeout(connect, 1500); // auto-reconnect
  };
  ws.onerror = () => ws.close();
  ws.onmessage = (e) => {
    let evt;
    try { evt = JSON.parse(e.data); } catch { return; }
    handleEvent(evt);
  };
}

function setConn(online) {
  // Show the reconnect banner only while disconnected (the app retries every ~1.5s).
  el.offlineBanner.hidden = online;
}

function sendWs(obj) {
  if (state.ws && state.ws.readyState === WebSocket.OPEN) {
    state.ws.send(JSON.stringify(obj));
  }
}

// ---------------------------------------------------------------------------
// Event handling (control frames + Claude stream-json)
// ---------------------------------------------------------------------------
function handleEvent(evt) {
  if (evt.cwi) {
    switch (evt.cwi) {
      case "session":
        state.sessionId = evt.session_id;
        // A replay means we're (re)attaching to a live session: rebuild the
        // view from the scrollback that follows.
        if (evt.replay) resetMessages();
        break;
      case "user":
        // A user turn, echoed by the keeper (also seen by other viewers).
        // Only finalize a *previous* assistant turn; don't reset the composer
        // for our own just-sent message (no current turn yet).
        if (state.current) finalizeTurn();
        addUserMessage(evt.text || "");
        break;
      case "exit":
        finalizeTurn();
        break;
      case "no_session":
        break; // nothing live to attach to; keep whatever is shown
      case "error":
        showSystem(`Ошибка: ${evt.message}`);
        finalizeTurn();
        break;
    }
    return;
  }

  switch (evt.type) {
    case "system":
      if (evt.subtype === "init" && evt.session_id) {
        if (!state.sessionId) state.sessionId = evt.session_id;
      }
      break;

    case "stream_event":
      handleStreamEvent(evt.event);
      break;

    case "result":
      if (evt.total_cost_usd != null || evt.duration_ms != null) {
        addMeta(evt);
      }
      finalizeTurn();
      break;
  }
}

function handleStreamEvent(ev) {
  if (!ev || !ev.type) return;
  const cur = ensureAssistant();

  switch (ev.type) {
    case "content_block_start": {
      // A new block begins: close the active text run.
      cur.textEl = null;
      const block = ev.content_block || {};
      if (block.type === "tool_use") {
        addToolChip(cur, block.name || "tool");
      } else if (block.type === "thinking") {
        ensureThinking(cur);
      }
      break;
    }
    case "content_block_delta": {
      const d = ev.delta || {};
      if (d.type === "text_delta" && d.text) {
        appendText(cur, d.text);
      } else if (d.type === "thinking_delta") {
        ensureThinking(cur);
        if (d.estimated_tokens != null) setThinkingTokens(cur, d.estimated_tokens);
        // Claude Code redacts thinking text in API mode, but honor it if present.
        if (d.thinking) appendThinking(cur, d.thinking);
      }
      break;
    }
  }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------
function ensureAssistant() {
  if (state.current) return state.current;
  clearEmptyState();
  const { msgEl, bodyEl } = makeMessage("assistant", "C");
  msgEl.classList.add("streaming");
  const answerEl = document.createElement("div");
  answerEl.className = "answer";
  bodyEl.appendChild(answerEl);
  state.current = { msgEl, bodyEl, answerEl, textEl: null, textRaw: "", thinkEl: null, thinkRaw: "" };
  state.streaming = true;
  setStreamingUI(true);
  return state.current;
}

function appendText(cur, text) {
  if (!cur.textEl) {
    cur.textEl = document.createElement("div");
    cur.textEl.className = "content cursor";
    cur.answerEl.appendChild(cur.textEl);
    cur.textRaw = "";
  }
  cur.textRaw += text;
  cur.textEl.innerHTML = renderMarkdown(cur.textRaw);
  scrollToBottom();
}

// Chevron icon (points down); rotated 180° via CSS when expanded.
const CHEVRON_SVG =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><path d="M6 9l6 6 6-6"/></svg>';

// Make `contentEl` collapse/expand (animated) via `chevronEl`. `rootEl` carries
// the `collapsed` state class (CSS controls the anchor: answers keep their last
// lines, the reasoning panel keeps its first lines).
function makeCollapsible(rootEl, contentEl, chevronEl) {
  rootEl.classList.add("cfold");
  contentEl.classList.add("fold-content");
  let collapsed = false;

  const collapsePx = () => {
    const lh = parseFloat(getComputedStyle(contentEl).lineHeight) || 22;
    return Math.round(lh * 2 + 6); // ~2 lines
  };

  function collapse(animate) {
    // Measure the natural height BEFORE switching to the collapsed layout
    // (which flips to flex and would report an inflated scrollHeight).
    const full = contentEl.scrollHeight;
    if (animate) {
      contentEl.style.maxHeight = full + "px";
      rootEl.classList.add("collapsed");
      requestAnimationFrame(() => {
        contentEl.style.maxHeight = collapsePx() + "px";
      });
    } else {
      rootEl.classList.add("collapsed");
      contentEl.style.maxHeight = collapsePx() + "px";
    }
    collapsed = true;
  }

  function expand() {
    contentEl.style.maxHeight = contentEl.scrollHeight + "px";
    rootEl.classList.remove("collapsed");
    const done = (e) => {
      if (e.propertyName !== "max-height") return;
      contentEl.style.maxHeight = ""; // back to natural height
      contentEl.removeEventListener("transitionend", done);
    };
    contentEl.addEventListener("transitionend", done);
    collapsed = false;
  }

  chevronEl.addEventListener("click", () => (collapsed ? expand() : collapse(true)));
  return { collapse, expand };
}

// One collapsible "thinking" panel per turn.
// Note: Claude Code redacts the verbatim thinking text in API/print mode, so
// usually only a token estimate is available; we still show the block honestly.
const THINK_NOTE =
  "Текст размышлений Claude Code не передаёт в режиме API — доступна только оценка объёма.";

function ensureThinking(cur) {
  if (cur.thinkEl) return cur.thinkEl;
  const panel = document.createElement("div");
  panel.className = "thinking";

  const head = document.createElement("div");
  head.className = "think-head";
  const label = document.createElement("span");
  label.className = "think-label";
  const chev = document.createElement("button");
  chev.className = "fold-chevron";
  chev.type = "button";
  chev.innerHTML = CHEVRON_SVG;
  chev.title = "Свернуть / развернуть размышления";
  head.appendChild(label);
  head.appendChild(chev);

  const content = document.createElement("div");
  content.className = "think-content";
  content.innerHTML = `<div class="think-note">${THINK_NOTE}</div>`;

  panel.appendChild(head);
  panel.appendChild(content);
  // Reasoning panel sits above the answer.
  cur.bodyEl.insertBefore(panel, cur.bodyEl.firstChild);

  cur.thinkEl = content;
  cur.thinkSummary = label;
  cur.thinkRaw = "";
  cur.thinkTokens = 0;
  renderThinkSummary(cur);

  // Chevron top-right; collapsed to the first ~2 lines by default.
  makeCollapsible(panel, content, chev).collapse(false);
  return cur.thinkEl;
}

function renderThinkSummary(cur) {
  if (!cur.thinkSummary) return;
  cur.thinkSummary.textContent = cur.thinkTokens
    ? `💭 Размышления · ~${cur.thinkTokens} токенов`
    : "💭 Размышления";
}

function setThinkingTokens(cur, n) {
  if (n > cur.thinkTokens) cur.thinkTokens = n;
  renderThinkSummary(cur);
}

function appendThinking(cur, text) {
  ensureThinking(cur);
  cur.thinkRaw += text;
  cur.thinkEl.innerHTML = renderMarkdown(cur.thinkRaw); // replaces the note
  scrollToBottom();
}

function addToolChip(cur, name) {
  const chip = document.createElement("span");
  chip.className = "tool-chip";
  chip.textContent = `⚙ ${name}`;
  cur.answerEl.appendChild(chip);
  scrollToBottom();
}

function addMeta(evt) {
  if (!state.current) return;
  const parts = [];
  if (evt.duration_ms != null) parts.push(`${(evt.duration_ms / 1000).toFixed(1)} с`);
  if (evt.total_cost_usd != null) parts.push(`$${evt.total_cost_usd.toFixed(4)}`);
  if (evt.num_turns != null) parts.push(`${evt.num_turns} turns`);
  if (!parts.length) return;
  const meta = document.createElement("div");
  meta.className = "meta-line";
  meta.textContent = parts.join(" · ");
  state.current.bodyEl.appendChild(meta);
}

function finalizeTurn() {
  if (state.current) {
    if (state.current.textEl) state.current.textEl.classList.remove("cursor");
    state.current.msgEl.classList.remove("streaming");
    if (state.current.answerEl) addFoldIfLong(state.current.answerEl);
  }
  state.current = null;
  state.streaming = false;
  setStreamingUI(false);
}

// Long answers get a chevron (bottom-right) to collapse to their last ~2 lines.
// Expanded by default.
const FOLD_THRESHOLD = 220; // px; roughly 9-10 lines
function addFoldIfLong(answerEl) {
  // scrollHeight forces a synchronous reflow, so this is accurate without rAF.
  if (answerEl.scrollHeight <= FOLD_THRESHOLD) return;
  if (answerEl.parentElement && answerEl.parentElement.classList.contains("foldable")) return;

  const wrap = document.createElement("div");
  wrap.className = "foldable answer";
  answerEl.replaceWith(wrap);
  wrap.appendChild(answerEl);

  const chev = document.createElement("button");
  chev.className = "fold-chevron";
  chev.type = "button";
  chev.innerHTML = CHEVRON_SVG;
  chev.title = "Свернуть / развернуть ответ";
  wrap.appendChild(chev);

  makeCollapsible(wrap, answerEl, chev); // starts expanded
}

function makeMessage(role, avatar) {
  const msgEl = document.createElement("div");
  msgEl.className = `msg ${role}`;

  const av = document.createElement("div");
  av.className = "avatar";
  av.textContent = avatar;

  const bodyEl = document.createElement("div");
  bodyEl.className = "body";

  msgEl.appendChild(av);
  msgEl.appendChild(bodyEl);
  el.messages.appendChild(msgEl);
  return { msgEl, bodyEl };
}

function addUserMessage(text) {
  clearEmptyState();
  const { bodyEl } = makeMessage("user", "U");
  const content = document.createElement("div");
  content.className = "content";
  content.textContent = text;
  bodyEl.appendChild(content);
  scrollToBottom();
}

function showSystem(text) {
  const div = document.createElement("div");
  div.className = "msg assistant";
  div.innerHTML = `<div class="avatar">!</div><div class="body"><div class="content">${escapeHtml(text)}</div></div>`;
  el.messages.appendChild(div);
  scrollToBottom();
}

function clearEmptyState() {
  const es = el.messages.querySelector(".empty-state");
  if (es) es.remove();
}

// Wipe the transcript view (used when replaying a live session on (re)attach).
function resetMessages() {
  el.messages.innerHTML = "";
  state.current = null;
}

function scrollToBottom() {
  el.messages.scrollTop = el.messages.scrollHeight;
}

function setStreamingUI(on) {
  el.input.disabled = false;
  updateSendButton();
}

// Show the send arrow only when there is text; the stop square only while streaming.
function updateSendButton() {
  el.stop.hidden = !state.streaming;
  el.send.hidden = state.streaming || el.input.value.trim().length === 0;
}

// ---------------------------------------------------------------------------
// Composer
// ---------------------------------------------------------------------------
el.composer.addEventListener("submit", (e) => {
  e.preventDefault();
  submit();
});

el.input.addEventListener("keydown", (e) => {
  // Swapped: Enter inserts a newline, Shift+Enter sends.
  if (e.key === "Enter" && e.shiftKey) {
    e.preventDefault();
    submit();
  }
});

el.input.addEventListener("input", autoGrow);
function autoGrow() {
  // Both writes happen in one tick, so only the final px value is painted —
  // the CSS `transition: height` animates the upward growth smoothly.
  el.input.style.height = "auto";
  el.input.style.height = Math.min(el.input.scrollHeight, 220) + "px";
  updateSendButton();
}

function submit() {
  const text = el.input.value.trim();
  if (!text || state.streaming) return;

  // The user bubble is rendered from the keeper's echo ({cwi:"user"}), so it
  // shows consistently across every attached viewer and on replay.
  sendWs({
    type: "send",
    session_id: state.sessionId,
    text,
    model: el.model.value || null,
    new_chat: state.isNew,
  });
  state.isNew = false; // subsequent turns reuse the live process

  el.input.value = "";
  autoGrow();
  state.streaming = true; // lock the composer until the turn completes
  setStreamingUI(true);
}

el.stop.addEventListener("click", () => {
  sendWs({ type: "interrupt" });
});

el.newChat.addEventListener("click", () => openNewChatModal());

// ---------------------------------------------------------------------------
// New chat modal (name + icon chosen up front)
// ---------------------------------------------------------------------------
let modalIcon = null;

function buildIconGrid() {
  el.iconGrid.innerHTML = "";
  const none = document.createElement("div");
  none.className = "icon-cell none selected";
  none.textContent = "без";
  none.title = "Без иконки";
  none.addEventListener("click", () => selectIcon(none, null));
  el.iconGrid.appendChild(none);

  for (const ic of ICONS) {
    const cell = document.createElement("div");
    cell.className = "icon-cell";
    cell.textContent = ic;
    cell.addEventListener("click", () => selectIcon(cell, ic));
    el.iconGrid.appendChild(cell);
  }
}

function selectIcon(cell, icon) {
  modalIcon = icon;
  el.iconGrid.querySelectorAll(".icon-cell.selected").forEach((n) => n.classList.remove("selected"));
  cell.classList.add("selected");
}

function openNewChatModal() {
  setSidebar(false); // close the drawer behind the modal
  buildIconGrid();
  modalIcon = null;
  el.chatName.value = "";
  el.modalCreate.disabled = true;
  el.modalOverlay.hidden = false;
  el.chatName.focus();
}

function closeNewChatModal() {
  el.modalOverlay.hidden = true;
}

el.chatName.addEventListener("input", () => {
  el.modalCreate.disabled = el.chatName.value.trim().length === 0;
});
el.chatName.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && el.chatName.value.trim()) {
    e.preventDefault();
    createChat();
  } else if (e.key === "Escape") {
    closeNewChatModal();
  }
});
el.modalCreate.addEventListener("click", createChat);
el.modalCancel.addEventListener("click", closeNewChatModal);
el.modalOverlay.addEventListener("click", (e) => {
  if (e.target === el.modalOverlay) closeNewChatModal();
});
// Escape closes the modal regardless of where focus is.
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && !el.modalOverlay.hidden) closeNewChatModal();
});

async function createChat() {
  const title = el.chatName.value.trim();
  if (!title) return;

  // Pre-assign a session id so the metadata can be saved before the first turn.
  const id = crypto.randomUUID();
  await saveMeta(id, title, modalIcon);

  // Open the new (empty) chat and focus the composer.
  state.sessionId = id;
  state.isNew = true;
  state.current = null;
  state.streaming = false;
  setStreamingUI(false);
  el.title.textContent = (modalIcon ? modalIcon + "  " : "") + title;
  el.messages.innerHTML =
    '<div class="empty-state"><h1>' +
    (modalIcon ? modalIcon + " " : "") +
    escapeHtml(title) +
    "</h1><p>Новый чат создан. Напишите первое сообщение.</p></div>";
  document.querySelectorAll(".chat-item.active").forEach((n) => n.classList.remove("active"));

  closeNewChatModal();
  setSidebar(false);
  refreshComposerState();
  el.input.focus();
  loadChatList();
}

async function saveMeta(id, title, icon) {
  try {
    await fetch(`/api/chats/${encodeURIComponent(id)}/meta`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ title, icon }),
    });
  } catch (e) {
    // ignore; UI already reflects the choice
  }
}

// ---------------------------------------------------------------------------
// Settings drawer (theme + font size)
// ---------------------------------------------------------------------------
const settings = {
  theme: localStorage.getItem("cwi_theme") || "dark",
  fontSize: localStorage.getItem("cwi_fontsize") || "15",
  model: localStorage.getItem("cwi_model") || "",
};

function applySettings() {
  document.documentElement.dataset.theme = settings.theme;
  // Root font-size — the whole page is sized in rem, so it all scales.
  document.documentElement.style.fontSize = settings.fontSize + "px";
  el.model.value = settings.model;
  markActive(el.themeSeg, "theme", settings.theme);
  markActive(el.fontSeg, "size", settings.fontSize);
  autoGrow(); // recompute the input height for the new font size
}

el.model.addEventListener("change", () => {
  settings.model = el.model.value;
  localStorage.setItem("cwi_model", settings.model);
});

// Populate the model dropdown from the Anthropic Models API (served by the
// backend, cached ~daily). Falls back to the static aliases if the request fails.
async function loadModels() {
  try {
    const res = await fetch("/api/models");
    const data = await res.json();
    if (data.models && data.models.length) populateModels(data.models);
  } catch (e) {
    // keep the static fallback options
  }
}
function populateModels(list) {
  const current = el.model.value || settings.model;
  el.model.innerHTML =
    '<option value="">по умолчанию</option>' +
    list
      .map((m) => `<option value="${escapeHtml(m.id)}">${escapeHtml(m.display_name || m.id)}</option>`)
      .join("");
  if ([...el.model.options].some((o) => o.value === current)) el.model.value = current;
}

function markActive(seg, attr, value) {
  seg.querySelectorAll(".seg-btn").forEach((b) => {
    b.classList.toggle("active", b.dataset[attr] === value);
  });
}

el.themeSeg.addEventListener("click", (e) => {
  const btn = e.target.closest(".seg-btn");
  if (!btn) return;
  settings.theme = btn.dataset.theme;
  localStorage.setItem("cwi_theme", settings.theme);
  applySettings();
});
el.fontSeg.addEventListener("click", (e) => {
  const btn = e.target.closest(".seg-btn");
  if (!btn) return;
  settings.fontSize = btn.dataset.size;
  localStorage.setItem("cwi_fontsize", settings.fontSize);
  applySettings();
});

// While a drawer is open its badge is hidden (immediately); on close, the badge
// fades back in shortly after the panel has slid away.
function hideBadge(badge) {
  badge.style.transition = "none";
  badge.style.opacity = "0";
  badge.style.pointerEvents = "none";
}
function showBadgeSoon(badge) {
  setTimeout(() => {
    badge.style.transition = "opacity .3s ease";
    badge.style.opacity = "1";
    badge.style.pointerEvents = "";
  }, 500);
}

// Settings drawer (right) — open via the gear, close by clicking outside.
function setSettings(open) {
  el.settingsPanel.classList.toggle("open", open);
  el.settingsOverlay.hidden = !open;
  if (open) hideBadge(el.settingsBadge);
  else showBadgeSoon(el.settingsBadge);
}
el.settingsBadge.addEventListener("click", () => setSettings(true));
el.settingsOverlay.addEventListener("click", () => setSettings(false));

// Chat list drawer (left) — mirrors the settings drawer.
function setSidebar(open) {
  el.sidebar.classList.toggle("open", open);
  el.sidebarOverlay.hidden = !open;
  if (open) hideBadge(el.sidebarBadge);
  else showBadgeSoon(el.sidebarBadge);
}
el.sidebarBadge.addEventListener("click", () => setSidebar(true));
el.sidebarOverlay.addEventListener("click", () => setSidebar(false));

// Composer + title show only when a chat is open. With no chat open, reveal the
// list; if there are no chats at all, offer a big "create" button instead.
function refreshComposerState() {
  const open = !!state.sessionId;
  const chatsExist = el.chatList.children.length > 0;
  el.composer.style.display = open ? "" : "none";
  el.title.style.display = open ? "" : "none";
  el.bigNewChat.hidden = open || chatsExist;
  updateSendButton();
}
el.bigNewChat.addEventListener("click", openNewChatModal);

// ---------------------------------------------------------------------------
// Chat list / history
// ---------------------------------------------------------------------------
async function loadChatList() {
  try {
    const res = await fetch("/api/chats");
    const chats = await res.json();
    renderChatList(chats);
  } catch (e) {
    // ignore
  }
}

function renderChatList(chats) {
  el.chatList.innerHTML = "";
  for (const c of chats) {
    const item = document.createElement("div");
    item.className = "chat-item";
    item.dataset.id = c.id;
    item.dataset.title = c.title;
    item.dataset.icon = c.icon || "";

    const row = document.createElement("div");
    row.className = "chat-title-row";

    if (c.icon) {
      const icon = document.createElement("span");
      icon.className = "chat-icon";
      icon.textContent = c.icon;
      row.appendChild(icon);
    }

    const title = document.createElement("div");
    title.className = "chat-title-text";
    title.textContent = c.title;
    row.appendChild(title);

    item.appendChild(row);
    item.addEventListener("click", () => openChat(c.id, item.dataset.title, item.dataset.icon, item));
    el.chatList.appendChild(item);
  }
  refreshComposerState();
}

async function openChat(id, title, icon, item) {
  if (state.streaming) return;
  setSidebar(false); // close the drawer once a chat is picked
  state.sessionId = id;
  state.isNew = false; // existing chat -> resume on next turn
  state.current = null;
  el.title.textContent = (icon ? icon + "  " : "") + title;
  document.querySelectorAll(".chat-item.active").forEach((n) => n.classList.remove("active"));
  if (item) item.classList.add("active");

  el.messages.innerHTML = "";
  try {
    const res = await fetch(`/api/chats/${encodeURIComponent(id)}`);
    const msgs = await res.json();
    if (!msgs.length) {
      showSystem("В этом чате пока нет сообщений.");
    }
    for (const m of msgs) {
      if (m.role === "user") {
        addUserMessage(m.text);
      } else {
        const { bodyEl } = makeMessage("assistant", "C");
        const answerEl = document.createElement("div");
        answerEl.className = "answer";
        const content = document.createElement("div");
        content.className = "content";
        content.innerHTML = renderMarkdown(m.text);
        answerEl.appendChild(content);
        bodyEl.appendChild(answerEl);
        addFoldIfLong(answerEl);
      }
    }
    scrollToBottom();
  } catch (e) {
    showSystem("Не удалось загрузить историю чата.");
  }

  refreshComposerState();
  el.input.focus(); // ready to type right after picking a chat

  // If this session is still running live, attach to it: the keeper will send a
  // {cwi:"session", replay:true} and rebuild the view from its live scrollback.
  sendWs({ type: "attach", session_id: id });
}

function fmtDate(iso) {
  try {
    const d = new Date(iso);
    return d.toLocaleDateString() + " " + d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  } catch {
    return "";
  }
}

// ---------------------------------------------------------------------------
// Minimal Markdown renderer (self-contained, HTML-escaping)
// ---------------------------------------------------------------------------
function escapeHtml(s) {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

function renderMarkdown(src) {
  const codeBlocks = [];
  // Fenced code blocks (support unterminated fence while streaming).
  src = src.replace(/```([^\n`]*)\n([\s\S]*?)(?:```|$)/g, (m, lang, code) => {
    const idx = codeBlocks.length;
    codeBlocks.push({ lang: lang.trim(), code });
    return `CODE${idx}`;
  });

  let html = escapeHtml(src);

  // Inline code
  html = html.replace(/`([^`\n]+)`/g, (m, c) => `<code class="inline">${c}</code>`);

  // Bold / italic
  html = html.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
  html = html.replace(/(^|[^*])\*([^*\n]+)\*/g, "$1<em>$2</em>");

  // Links [text](url)
  html = html.replace(/\[([^\]]+)\]\((https?:\/\/[^\s)]+)\)/g,
    '<a href="$2" target="_blank" rel="noopener">$1</a>');

  // Block-level: headings, lists, paragraphs
  const lines = html.split("\n");
  const out = [];
  let listType = null; // "ul" | "ol"

  const closeList = () => {
    if (listType) { out.push(`</${listType}>`); listType = null; }
  };

  for (let raw of lines) {
    const line = raw.trimEnd();

    if (/^CODE\d+$/.test(line.trim())) {
      closeList();
      out.push(line.trim());
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
  html = html.replace(/CODE(\d+)/g, (m, i) => {
    const { lang, code } = codeBlocks[+i];
    const escaped = escapeHtml(code.replace(/\n$/, ""));
    const langLabel = lang ? ` data-lang="${escapeHtml(lang)}"` : "";
    return `<pre${langLabel}><button class="copy-btn" onclick="copyCode(this)">копировать</button><code>${escaped}</code></pre>`;
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

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------
applySettings();
connect();
loadChatList();
loadModels();
refreshComposerState();
// No chat open on load → reveal the chat list drawer.
if (!state.sessionId) setSidebar(true);
el.input.focus();
// Refresh chat list periodically so new/updated chats appear.
setInterval(loadChatList, 15000);
