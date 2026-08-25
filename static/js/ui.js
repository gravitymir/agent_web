import { state, el, ICONS, escapeHtml, renderMarkdown, chatFrozen } from './state.js';
import { iIcon } from './ios-icons.js';
import { connect, sendWs, markSentPending } from './ws.js';
import { renderToolCard, addFoldIfLong, makeMessage, addUserMessage, showSystem, resetMessages, renderMsgRange, scrollToBottom, scrollToBottomIfPinned, updateScrollbar, updateScrollToBottomButton, setStreamingUI, updateUsageBadge, setUsage, updateSendButton, renderAttachPreview, loadUsage, estimateTokens } from './render.js';
import { setFaviconState } from '../favicon.js';
import { ensureNotifyPermission } from '../notify.js';
import { playDictationStart, playDictationStop } from '../sound.js';
// ---------------------------------------------------------------------------
// Composer
// ---------------------------------------------------------------------------
el.composer.addEventListener("submit", (e) => {
  e.preventDefault();
  submit();
});

el.input.addEventListener("keydown", (e) => {
  if (e.key !== "Enter") return;

  // While dictating, Enter only stops the recording — it does not send. The
  // recognized text stays in the field for review; a *second*, separate Enter
  // (now that dictation is off) sends it, same as any other typed message.
  if (dictation.active) {
    e.preventDefault();
    stopDictation();
    return;
  }
  // Standard chat behaviour: Shift+Enter inserts a newline, plain Enter sends.
  if (e.shiftKey) return; // let the default newline through
  e.preventDefault();
  submit();
});

el.input.addEventListener("input", autoGrow);
export function autoGrow() {
  // When empty, fall back to the CSS one-row height. Chromium counts the (long,
  // wrapping) placeholder in scrollHeight for an empty textarea, which would
  // otherwise make the field 2-3 lines tall until the first character is typed.
  if (el.input.value === "") {
    el.input.style.height = "";
    updateSendButton();
    return;
  }
  // Both writes happen in one tick, so only the final px value is painted —
  // the CSS `transition: height` animates the upward growth smoothly.
  el.input.style.height = "auto";
  el.input.style.height = Math.min(el.input.scrollHeight, 220) + "px";
  updateSendButton();
}

export async function submit() {
  const typed = el.input.value.trim();
  const images = state.pendingImages;
  const files = state.pendingFiles;
  if (!typed && !images.length && !files.length) return;
  if (chatFrozen(state.sessionId)) {
    showSystem("Чат только для чтения — создан другим движком. Переключите CWI_ENGINE, чтобы продолжить.");
    return;
  }
  // Server is draining (graceful Drain-Stop): no new turns AND no queueing.
  // Let the in-flight answer finish; refuse everything else so the shutdown
  // actually converges. The backend also rejects new sends (ws.rs) — this is
  // the client-side half so nothing even lands in the queue.
  if (state.draining) {
    showSystem("Сервер останавливается — новые сообщения не принимаются. Дождитесь завершения текущего ответа.");
    return;
  }

  // Inline attached text files as fenced blocks ahead of the typed message, so
  // the model sees their contents (works for CLI and native engines alike).
  const text = filesToPrompt(files) + typed;
  const imgs = images.map((i) => ({ media_type: i.media_type, data: i.data }));

  // Context-budget guard: attached files are inlined into `text`, so a huge
  // attachment (or a chat whose resumed history is already near the model's
  // window) would only bounce off the API ("Context is too large…", seen on
  // Qwen: 1.3M prompt vs its 977k hard cap). Estimate BEFORE sending and let
  // the user decide. The limit comes from the chat's own stats (Qwen stamps
  // 1M per line; Claude defaults to 200k); 0.9 leaves room for the reply and
  // for the estimator's roughness (~4 chars/token undercounts Cyrillic).
  {
    const u = state.chatUsage[state.sessionId] || {};
    const limit = u.contextLimit || (state.cliFlavor === "qwen" ? 1_000_000 : 200_000);
    const used = u.contextTokens || 0;
    const est = estimateTokens(text) + imgs.length * 1500;
    if (used + est > limit * 0.9) {
      const fmt = (n) => (n >= 1e6 ? (n / 1e6).toFixed(1) + "M" : Math.round(n / 1000) + "k");
      const ok = await confirmChatAction({
        title: "Не влезет в контекст модели",
        message: `Новое сообщение ~${fmt(est)} токенов + уже в контексте ~${fmt(used)}, а окно модели ~${fmt(limit)}. Скорее всего API откажет («Context is too large»). Уменьшите файлы или начните новый чат.`,
        confirmLabel: "Отправить всё равно",
        danger: true,
      });
      if (!ok) return; // composer untouched — trim the files or start fresh
    }
  }

  // A turn is already streaming → queue this message instead of blocking. The
  // user keeps typing; the queue drains one message per turn (`flushQueue` on
  // turn end). Clear the composer now — the queued copy lives in the strip.
  if (state.streaming) {
    state.queue.push({ text, imgs, label: typed || `📎 ${images.length + files.length}` });
    renderQueue();
    el.input.value = "";
    state.pendingImages = [];
    state.pendingFiles = [];
    renderAttachPreview();
    autoGrow();
    updateSendButton();
    return;
  }

  if (!sendMessage(text, imgs)) {
    // Nothing was touched — input and attachments are exactly as the user
    // left them. No auto-retry: they decide when to try again themselves.
    showSystem("Соединение потеряно — сообщение не отправлено. Отправьте ещё раз, когда связь восстановится.");
    return;
  }

  // `readyState` can still read OPEN for a moment after the server process
  // actually died — so this "success" isn't guaranteed yet. Hold the text AND
  // attachments (don't clear them) and lock the composer until the keeper's own
  // echo confirms the send arrived (`confirmSentMessage`) — or, if the
  // connection drops first, just unlock it again (`restoreUnsentMessage`).
  markSentPending();
  lockComposerForConfirmation(true);
  scrollToBottom();
}

// Build the send payload and push it over the socket, starting a turn. Returns
// whether it actually went out. Deliberately does NOT touch the composer, so it
// serves both a fresh typed send (submit holds the composer for confirmation)
// and a queued-message flush (composer is free for the user's next message).
function sendMessage(text, imgs) {
  const payload = {
    type: "send",
    session_id: state.sessionId,
    text,
    model: el.model.value || null,
    provider: settings.provider || null,
    new_chat: state.isNew,
    images: imgs,
    caps: currentCaps(),
  };
  if (!sendWs(payload)) return false;
  // First message of a brand-new chat → the backend created the session file;
  // refresh the sidebar so it survives a reload.
  if (state.isNew) loadChatList();
  state.isNew = false;
  state.streaming = true;
  setStreamingUI(true);
  updateSendButton(); // toggle send → stop
  return true;
}

// Drain the queue by one when the current turn ends (called from render.js's
// turn-end path). Re-queues and pauses on a failed send.
export async function flushQueue() {
  // Never start a new turn while draining — the current answer just finished,
  // and the whole point of Drain-Stop is to stop here. Queued chips stay put
  // (frozen) so the user sees what didn't go out.
  if (state.streaming || state.draining || !state.queue.length) return;
  // Authoritative drain check at the exact moment we'd fire the next turn — the
  // 15s health poll may not have caught a drain that began mid-answer, and this
  // is the one point where it actually matters. If the server is draining now,
  // freeze the queue here (surface the notice; the backend would reject it too).
  if (await isDraining()) {
    state.draining = true;
    el.drainNotice.hidden = false;
    updateSendButton();
    return;
  }
  const next = state.queue.shift();
  renderQueue();
  if (sendMessage(next.text, next.imgs)) {
    scrollToBottom();
  } else {
    state.queue.unshift(next);
    renderQueue();
    showSystem("Соединение потеряно — очередь на паузе.");
  }
}

// Fresh, blocking read of the server's drain state. Falls back to the last
// known flag if health can't be reached, so a transient blip doesn't flush.
async function isDraining() {
  try {
    const r = await fetch("/api/health");
    if (r.ok) return !!(await r.json()).draining;
  } catch { /* fall through */ }
  return state.draining;
}

// Render the pending-message strip. Each chip is the queued text + a ✕ to drop
// it before it's sent.
export function renderQueue() {
  el.queueStrip.hidden = state.queue.length === 0;
  el.queueStrip.innerHTML = state.queue
    .map(
      (m, i) =>
        `<span class="queue-chip"><span class="queue-chip-txt">${escapeHtml(m.label.slice(0, 140))}</span>` +
        `<button type="button" class="queue-chip-x" data-i="${i}" aria-label="Убрать из очереди">✕</button></span>`
    )
    .join("");
}
el.queueStrip.addEventListener("click", (e) => {
  const btn = e.target.closest && e.target.closest(".queue-chip-x");
  if (btn) {
    state.queue.splice(Number(btn.dataset.i), 1);
    renderQueue();
  }
});

// Locks/unlocks just the "is this pending send confirmed yet" window — a
// narrower, shorter scope than `state.streaming` (which covers the whole
// turn): the user can still queue up their NEXT message once THIS one is
// confirmed, even while the assistant is still generating a reply.
function lockComposerForConfirmation(locked) {
  el.input.disabled = locked;
  el.attachPreview.querySelectorAll(".rm").forEach((btn) => { btn.disabled = locked; });
}

// Called from ws.js once the server's own echo confirms a pending send truly
// arrived — only now is it safe to actually clear the composer.
export function confirmSentMessage() {
  lockComposerForConfirmation(false);
  el.input.value = "";
  state.pendingImages = [];
  state.pendingFiles = [];
  renderAttachPreview();
  autoGrow();
}

// Called from ws.js when a send that looked successful never got confirmed —
// the connection dropped before its echo arrived. Text/attachments were never
// cleared, so there's nothing to restore — just unlock the composer.
export function restoreUnsentMessage() {
  lockComposerForConfirmation(false);
  showSystem("Соединение прервалось до подтверждения отправки — сообщение не ушло. Отправьте ещё раз, когда связь восстановится.");
}

// Turn attached text files into a prompt preamble. Uses a fence long enough to
// never collide with backticks in the file's own content.
export function filesToPrompt(files) {
  if (!files || !files.length) return "";
  let out = "";
  for (const f of files) {
    const fence = "```";
    const ext = (f.name.match(/\.([a-z0-9]+)$/i) || [, ""])[1].toLowerCase();
    out += `Файл \`${f.name}\`:\n${fence}${ext}\n${f.text}\n${fence}\n`;
    if (f.truncated) out += `_(файл обрезан до лимита)_\n`;
    out += "\n";
  }
  return out;
}

el.stop.addEventListener("click", () => {
  // Stop needs the live socket (the keeper runs server-side). If it can't be
  // delivered, say so instead of failing silently.
  if (!sendWs({ type: "interrupt" })) {
    showSystem("Нет связи с сервером — остановить не удалось. Попробуйте после переподключения.");
  }
});

// ---------------------------------------------------------------------------
// Tool permissions. The tools/permissions panel was removed — every built-in
// tool group is always permitted. The guest sandbox ignores caps (it runs
// bypassPermissions with only mcp__guest__* tools); the owner CLI auto-approves.
// `caps` is still sent so the backend has an explicit (all-allowed) value.
// ---------------------------------------------------------------------------
export function currentCaps() {
  return { web_fetch: true, web_search: true, read: true, modify: true, run: true };
}

// ---------------------------------------------------------------------------
// Dictation (speech-to-text via the browser's Web Speech API)
// Claude has no audio input, so — like the desktop app — we transcribe locally
// and drop the recognized text into the input; the user reviews and sends.
// ---------------------------------------------------------------------------
export const SpeechRec = window.SpeechRecognition || window.webkitSpeechRecognition;
// `finalText` is every finalized word of the current dictation, concatenated —
// see takeFinalDelta for why we track it instead of trusting `resultIndex`.
export const dictation = { rec: null, active: false, interim: "", finalText: "" };

// Return only the newly finalized text in this event, and remember the total.
//
// Why not `e.resultIndex`: per spec `results` accumulates every result of the
// session and `resultIndex` marks the first changed one, so iterating from it
// should yield only new words. Chrome on Android with `continuous: true` breaks
// that — it re-delivers already-final results with the index back near 0, so
// iterating from `resultIndex` re-inserted the whole phrase on every event.
// That is the "смотри / смотри ещё / смотри ещё какие-то …" pile-up: each event
// appended everything said so far, again.
//
// So: compare against what we actually committed. The accumulated list is a
// growing prefix, so the delta is the tail. If the new text is NOT a
// continuation (an engine that restarts its result list after a pause), treat
// all of it as new instead of silently dropping it.
export function takeFinalDelta(results) {
  let all = "";
  for (const r of results) {
    if (r.isFinal) all += r[0].transcript;
  }
  const delta = all.startsWith(dictation.finalText)
    ? all.slice(dictation.finalText.length)
    : all;
  dictation.finalText = all;
  return delta;
}

/** The current non-final tail (what the engine is still refining). */
export function interimText(results) {
  let interim = "";
  for (const r of results) {
    if (!r.isFinal) interim += r[0].transcript;
  }
  return interim;
}

// Insert text at the current caret (replacing any selection), gluing on a space
// if it would butt against a word. Returns exactly what was inserted.
export function dictInsert(text) {
  if (!text) return "";
  const v = el.input.value;
  const s = el.input.selectionStart, e = el.input.selectionEnd;
  const needSpace = s > 0 && !/\s$/.test(v.slice(0, s)) && !/^\s/.test(text);
  const ins = (needSpace ? " " : "") + text;
  el.input.value = v.slice(0, s) + ins + v.slice(e);
  const caret = s + ins.length;
  el.input.selectionStart = el.input.selectionEnd = caret;
  return ins;
}

// Pull the last interim string back out — but only if it still sits untouched
// right before the caret. If the user edited or moved away, we leave their text
// alone (and just forget the stale interim), so nothing is ever "restored".
export function removeInterim() {
  const s = dictation.interim;
  if (!s) return;
  const v = el.input.value;
  const caret = el.input.selectionStart;
  const start = caret - s.length;
  if (start >= 0 && v.slice(start, caret) === s) {
    el.input.value = v.slice(0, start) + v.slice(caret);
    el.input.selectionStart = el.input.selectionEnd = start;
  }
  dictation.interim = "";
}

if (SpeechRec) {
  el.mic.hidden = false; // reveal the button only where recognition is supported
  el.mic.addEventListener("click", () =>
    dictation.active ? stopDictation() : startDictation()
  );
}

export function startDictation() {
  if (!SpeechRec || dictation.active) return;
  const rec = new SpeechRec();
  rec.lang = "ru-RU";
  rec.interimResults = true;
  rec.continuous = true;

  dictation.interim = "";
  dictation.finalText = "";
  el.input.focus(); // make the caret live so inserts land where it sits

  rec.onresult = (e) => {
    // Take out the previously shown interim, then re-insert this event's words
    // at the *current* caret: final words commit permanently, interim is the
    // replaceable tail we'll remove next time.
    removeInterim();
    const finalChunk = takeFinalDelta(e.results);
    const interim = interimText(e.results);
    if (finalChunk) dictInsert(finalChunk);
    if (interim) dictation.interim = dictInsert(interim);
    autoGrow();
  };
  rec.onend = () => endDictationUI(true);
  rec.onerror = () => endDictationUI(false);

  try {
    rec.start();
  } catch (_) {
    return; // e.g. already started
  }
  dictation.rec = rec;
  dictation.active = true;
  el.mic.classList.add("recording");
  el.mic.title = "Остановить диктовку";
  playDictationStart(); // short rising cue: voice input is now listening
}

export function stopDictation() {
  if (dictation.rec) dictation.rec.stop();
}

export function endDictationUI(ok) {
  dictation.active = false;
  dictation.rec = null;
  dictation.interim = ""; // any tail left in the field stays as real text
  dictation.finalText = ""; // next dictation starts its own accumulation
  el.mic.classList.remove("recording");
  el.mic.title = "Диктовка (голосовой ввод)";
  playDictationStop(); // short falling cue: voice input has stopped
  // Only the explicit auto-send setting sends automatically — stopping
  // dictation (via Enter, the mic button, or recognition ending on its own)
  // never sends by itself; the user reviews the text and sends separately.
  if (ok && settings.autoSend && el.input.value.trim()) {
    submit();
    return;
  }
  el.input.focus();
}

// ---------------------------------------------------------------------------
// Quick-prompt templates (shown in a new chat's empty state).
// ---------------------------------------------------------------------------
export const QUICK_PROMPTS = [
  { icon: "book", label: "Объясни код", text: "Объясни, что делает этот код и как он устроен:\n\n" },
  { icon: "bug", label: "Найди баги", text: "Просмотри код на предмет багов и потенциальных проблем." },
  { icon: "recycle", label: "Рефакторинг", text: "Предложи рефакторинг этого кода, сохранив поведение:\n\n" },
  { icon: "flask", label: "Напиши тесты", text: "Напиши тесты для этого кода." },
];

export function buildQuickPrompts() {
  const wrap = document.createElement("div");
  wrap.className = "quick-prompts";
  for (const q of QUICK_PROMPTS) {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "quick-prompt";
    btn.innerHTML = `${iIcon(q.icon, 16, 'inline')} ${q.label}`;
    btn.addEventListener("click", () => insertPrompt(q.text));
    wrap.appendChild(btn);
  }
  return wrap;
}

export function insertPrompt(text) {
  el.input.value = text;
  autoGrow();
  el.input.focus();
  // Place the caret at the end so the user types right where the template leaves off.
  el.input.selectionStart = el.input.selectionEnd = el.input.value.length;
}

// ---------------------------------------------------------------------------
// Current-chat actions (export MD / JSON, archive, delete). They used to sit in
// a floating chip over the bottom-right of the chat; now they live in a right
// drawer opened by the download badge, so nothing overlaps the transcript.
// ---------------------------------------------------------------------------
function currentChat() {
  const active = document.querySelector(".chat-item.active");
  if (active) {
    return { id: active.dataset.id, title: active.dataset.title, icon: active.dataset.icon || null };
  }
  // Brand-new chat not yet in the list: fall back to the open session/title.
  return { id: state.sessionId, title: (el.titleName.textContent || "").trim(), icon: null };
}
document.getElementById("ca-export-md").addEventListener("click", () => {
  const c = currentChat();
  if (!c.id) return;
  setChatActions(false); // the confirmation dialog is the next step; free the panel
  confirmChatAction({
    title: "Экспорт в Markdown",
    message: `Скачать историю чата «${c.title}» в формате Markdown?`,
    confirmLabel: "Скачать",
  }).then((confirmed) => { if (confirmed) exportChat(c.id, c.title, c.icon); });
});
document.getElementById("ca-export-json").addEventListener("click", () => {
  const c = currentChat();
  if (!c.id) return;
  setChatActions(false);
  confirmChatAction({
    title: "Экспорт в JSON",
    message: `Скачать историю чата «${c.title}» в формате JSON?`,
    confirmLabel: "Скачать",
  }).then((confirmed) => { if (confirmed) exportChatJson(c.id, c.title); });
});
document.getElementById("ca-download-zip").addEventListener("click", () => {
  // Download the current chat's workspace. The server picks the source (guest →
  // this chat's files on the VM as tar.gz; owner → local workspace zip) and sets
  // the filename via Content-Disposition; the same-origin cookie authenticates.
  if (!state.sessionId) return; // nothing to download without an open chat
  setChatActions(false);
  const a = document.createElement("a");
  a.href = "/api/workspace.zip?chat=" + encodeURIComponent(state.sessionId);
  document.body.appendChild(a);
  a.click();
  a.remove();
});
document.getElementById("ca-delete").addEventListener("click", () => {
  const c = currentChat();
  if (!c.id) return;
  setChatActions(false);
  deleteChat(c.id, c.title, null);
});

let pendingChatAction = null;

function confirmChatAction({ title, message, confirmLabel, danger = false }) {
  if (pendingChatAction) return Promise.resolve(false);
  el.actionModalTitle.textContent = title;
  el.actionModalMessage.textContent = message;
  el.actionModalConfirm.textContent = confirmLabel;
  el.actionModalConfirm.classList.toggle("btn-danger", danger);
  el.actionModalOverlay.hidden = false;
  const trigger = document.activeElement;
  el.actionModalCancel.focus();
  return new Promise((resolve) => { pendingChatAction = { resolve, trigger }; });
}

function closeChatActionConfirmation(confirmed) {
  if (!pendingChatAction) return;
  const { resolve, trigger } = pendingChatAction;
  pendingChatAction = null;
  el.actionModalOverlay.hidden = true;
  if (trigger instanceof HTMLElement) trigger.focus();
  resolve(confirmed);
}

el.actionModalConfirm.addEventListener("click", () => closeChatActionConfirmation(true));
el.actionModalCancel.addEventListener("click", () => closeChatActionConfirmation(false));
el.actionModalOverlay.addEventListener("click", (e) => {
  if (e.target === el.actionModalOverlay) closeChatActionConfirmation(false);
});
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && !el.actionModalOverlay.hidden) closeChatActionConfirmation(false);
});

el.newChat.addEventListener("click", () => openNewChatModal());

// ---------------------------------------------------------------------------
// New chat modal (name + icon chosen up front)
// ---------------------------------------------------------------------------
export let modalIcon = null;

export function buildIconGrid() {
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
    cell.innerHTML = iIcon(ic, 22);
    cell.dataset.icon = ic;
    cell.addEventListener("click", () => selectIcon(cell, ic));
    el.iconGrid.appendChild(cell);
  }
}

export function selectIcon(cell, icon) {
  modalIcon = icon;
  el.iconGrid.querySelectorAll(".icon-cell.selected").forEach((n) => n.classList.remove("selected"));
  cell.classList.add("selected");
  // Move focus to the name field so the user can keep typing right after picking
  // an icon (the click otherwise leaves focus on the icon cell).
  el.chatName.focus();
}

export function openNewChatModal() {
  setSidebar(false); // close the drawer behind the modal
  buildIconGrid();
  modalIcon = null;
  el.chatName.value = "";
  el.modalCreate.disabled = true;
  el.modalOverlay.hidden = false;
  // Focus the name field. Try immediately, then again on a macrotask: focusing
  // in the same tick the overlay is unhidden often doesn't take (the input isn't
  // focusable until layout updates). setTimeout (not rAF) fires even when the tab
  // isn't compositing, so the focus lands reliably.
  el.chatName.focus();
  setTimeout(() => el.chatName.focus(), 0);
}

export function closeNewChatModal() {
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

export async function createChat() {
  const title = el.chatName.value.trim();
  if (!title) return;

  // Pre-assign a session id so the metadata can be saved before the first turn.
  const id = crypto.randomUUID();
  await saveMeta(id, title, modalIcon);

  // Open the new (empty) chat and focus the composer.
  state.sessionId = id;
  // Remember the newly created chat so it reopens after a page reload.
  try {
    localStorage.setItem("cwi_last_chat", id);
    localStorage.setItem("cwi_live_session", JSON.stringify({ id, ts: Date.now() }));
  } catch (e) {}
  state.isNew = true;
  state.current = null;
  // Drop any previously-open chat's transcript: without this, the new chat's
  // replay_end (a fresh keeper, !hadLiveReplay) re-renders the STALE transcript
  // as if it were this chat's history (see ws.js).
  state.transcript = null;
  state.queue = []; // queued messages belong to the chat you left
  renderQueue();
  state.streaming = false;
  setStreamingUI(false);
  setFaviconState("idle");
  el.titleName.innerHTML = (modalIcon ? iIcon(modalIcon, 18, "inline") + "  " : "") + escapeHtml(title);
  el.messages.innerHTML = "";
  const empty = document.createElement("div");
  empty.className = "empty-state";
  const h = document.createElement("h1");
  h.innerHTML = (modalIcon ? iIcon(modalIcon, 24, "inline") + " " : "") + escapeHtml(title);
  const p = document.createElement("p");
  p.textContent = "Новый чат создан. Напишите первое сообщение или начните с шаблона:";
  empty.appendChild(h);
  empty.appendChild(p);
  empty.appendChild(buildQuickPrompts());
  el.messages.appendChild(empty);
  document.querySelectorAll(".chat-item.active").forEach((n) => n.classList.remove("active"));

  closeNewChatModal();
  setSidebar(false);
  refreshComposerState();
  el.input.focus();
  loadChatList();
}

export async function saveMeta(id, title, icon) {
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
// Settings drawer (font size, model, voice)
// ---------------------------------------------------------------------------
export const settings = {
  fontSize: localStorage.getItem("cwi_fontsize") || "15",
  model: localStorage.getItem("cwi_model") || "",
  provider: localStorage.getItem("cwi_provider") || "",
  // Voice: auto-send after dictation. Off by default.
  autoSend: localStorage.getItem("cwi_autosend") === "1",
  // Chime when a turn finishes. On by default; absent key ("" from
  // localStorage.getItem returning null → not "0") reads as enabled.
  sound: localStorage.getItem("cwi_sound") !== "0",
  // Desktop notification when a turn finishes in a hidden tab. Off by default —
  // unlike sound, this needs an explicit OS permission grant.
  notify: localStorage.getItem("cwi_notify") === "1",
  // Context management (direct-API / Gemini engine only). Master on by default,
  // but auto-compaction off — so out of the box it only hints/offers, never acts.
  // Thresholds are bounded so nonsense values (100% context, 1% nudge) can't be set.
  ctxMgmt: localStorage.getItem("cwi_ctx_mgmt") !== "0",
  ctxNudge: clampPct(+(localStorage.getItem("cwi_ctx_nudge") || 65), 40, 85),
  ctxCompress: clampPct(+(localStorage.getItem("cwi_ctx_compress") || 95), 80, 98),
  ctxAuto: localStorage.getItem("cwi_ctx_auto") === "1",
};

// Clamp a percent into [lo, hi]; NaN falls back to lo.
function clampPct(v, lo, hi) {
  if (!Number.isFinite(v)) return lo;
  return Math.max(lo, Math.min(hi, Math.round(v)));
}

export function applySettings() {
  // Root font-size — the whole page is sized in rem, so it all scales.
  document.documentElement.style.fontSize = settings.fontSize + "px";
  el.model.value = settings.model;
  el.autosend.checked = settings.autoSend;
  el.sound.checked = settings.sound;
  el.notify.checked = settings.notify;
  el.ctxMgmt.checked = settings.ctxMgmt;
  el.ctxAuto.checked = settings.ctxAuto;
  el.ctxNudge.value = settings.ctxNudge;
  el.ctxCompress.value = settings.ctxCompress;
  syncCtxControls();
  markActive(el.fontSeg, "size", settings.fontSize);
  autoGrow(); // recompute the input height for the new font size
}

// Reflect ctx settings into the widgets: value labels + master toggle enabling
// or dimming the sub-controls.
export function syncCtxControls() {
  el.ctxNudgeVal.textContent = settings.ctxNudge + "%";
  el.ctxCompressVal.textContent = settings.ctxCompress + "%";
  const off = !settings.ctxMgmt;
  el.ctxControls.classList.toggle("disabled", off);
  [el.ctxNudge, el.ctxCompress, el.ctxAuto, el.ctxMore].forEach((c) => (c.disabled = off));
}

el.autosend.addEventListener("change", () => {
  settings.autoSend = el.autosend.checked;
  localStorage.setItem("cwi_autosend", settings.autoSend ? "1" : "0");
});

el.sound.addEventListener("change", () => {
  settings.sound = el.sound.checked;
  localStorage.setItem("cwi_sound", settings.sound ? "1" : "0");
});

el.notify.addEventListener("change", () => {
  settings.notify = el.notify.checked;
  localStorage.setItem("cwi_notify", settings.notify ? "1" : "0");
  if (!settings.notify) return;
  // The permission prompt requires a user gesture — this click is one. If the
  // browser didn't actually grant it (blocked earlier, or the prompt got
  // dismissed without a choice), silently leaving the checkbox "on" would be
  // misleading — notifications would never fire and nothing would say why.
  ensureNotifyPermission().then((permission) => {
    if (permission === "granted") return;
    settings.notify = false;
    el.notify.checked = false;
    localStorage.setItem("cwi_notify", "0");
    const why = permission === "denied"
      ? "уведомления заблокированы в настройках сайта в браузере — разрешите их там и включите ещё раз."
      : "браузер не подтвердил разрешение (диалог закрыли без ответа).";
    showSystem(`Не удалось включить уведомления: ${why}`);
  });
});

// --- Context management controls (direct-API / Gemini engine) --------------
el.ctxMgmt.addEventListener("change", () => {
  settings.ctxMgmt = el.ctxMgmt.checked;
  localStorage.setItem("cwi_ctx_mgmt", settings.ctxMgmt ? "1" : "0");
  syncCtxControls();
});
el.ctxAuto.addEventListener("change", () => {
  settings.ctxAuto = el.ctxAuto.checked;
  localStorage.setItem("cwi_ctx_auto", settings.ctxAuto ? "1" : "0");
});
el.ctxNudge.addEventListener("input", () => {
  let v = clampPct(+el.ctxNudge.value, 40, 85);
  if (v >= settings.ctxCompress) v = settings.ctxCompress - 5; // nudge stays below compress
  settings.ctxNudge = v;
  el.ctxNudge.value = v;
  localStorage.setItem("cwi_ctx_nudge", String(v));
  syncCtxControls();
});
el.ctxCompress.addEventListener("input", () => {
  const v = clampPct(+el.ctxCompress.value, 80, 98);
  settings.ctxCompress = v;
  if (settings.ctxNudge >= v) {
    settings.ctxNudge = Math.max(40, v - 5);
    el.ctxNudge.value = settings.ctxNudge;
    localStorage.setItem("cwi_ctx_nudge", String(settings.ctxNudge));
  }
  localStorage.setItem("cwi_ctx_compress", String(v));
  syncCtxControls();
});
el.ctxMore.addEventListener("click", () => {
  confirmChatAction({
    title: "Управление контекстом",
    message:
      "Каждый ход агент заново шлёт модели весь накопленный контекст, поэтому чем длиннее чат, тем дороже каждый шаг (в токенах и лимитах). " +
      "«Подсказка» предупредит, когда контекст заполнится. «Сжатие» сворачивает старые ходы в краткое резюме, чтобы дальше слать меньше. " +
      "Сжатие выполняется только когда ход завершён — не посреди работы. Работает для прямого API (Gemini); у Cloud CLI своё авто-сжатие.",
    confirmLabel: "Понятно",
  });
});

el.model.addEventListener("change", () => {
  settings.model = el.model.value;
  localStorage.setItem("cwi_model", settings.model);
});

// Populate the model dropdown from the Anthropic Models API (served by the
// backend, cached ~daily). Falls back to the static aliases if the request fails.
export async function loadModels() {
  try {
    const res = await fetch("/api/models");
    const data = await res.json();
    if (data.models && data.models.length) populateModels(data.models);
  } catch (e) {
    // keep the static fallback options
  }
}
export function populateModels(list) {
  const current = el.model.value || settings.model;
  el.model.innerHTML =
    '<option value="">по умолчанию</option>' +
    list
      .map((m) => `<option value="${escapeHtml(m.id)}">${escapeHtml(m.display_name || m.id)}</option>`)
      .join("");
  if ([...el.model.options].some((o) => o.value === current)) el.model.value = current;
}

// Native engine: pick a provider first, then a model of that provider.
export async function loadProviders() {
  let data;
  try {
    data = await (await fetch("/api/providers")).json();
  } catch (e) {
    loadModels();
    return;
  }
  // Now that we know the active engine, mark cross-engine chats as frozen.
  state.engineNative = !!data.native;
  state.cliFlavor = data.cli_flavor || "claude";
  redecorateChatList();
  refreshComposerState();
  if (!data.native) {
    el.providerSection.hidden = true;
    el.ctxSection.hidden = true; // context mgmt is direct-API only; Cloud CLI self-compacts
    renderEngineBadge();
    loadModels(); // CLI mode: Claude models via the OAuth-backed endpoint
    return;
  }
  el.providerSection.hidden = false;
  el.ctxSection.hidden = false;
  state.providers = data.providers || [];
  state.activeProvider = data.active || "";
  state.activeModel = data.active_model || "";
  populateProviders();
  renderEngineBadge();
}

// The active-engine label now lives as the header of the usage badge (top-right):
// "Cloud <plan>" in subscription/CLI mode, the provider name (e.g. "Gemini") in
// native mode. Kept as a named export so its existing callers just refresh the
// merged badge whenever the engine/provider/model changes.
export function renderEngineBadge() {
  updateUsageBadge();
}

export function populateProviders() {
  const list = state.providers;
  el.provider.innerHTML = list
    .map(
      (p) =>
        `<option value="${escapeHtml(p.id)}"${p.has_key ? "" : " disabled"}>${escapeHtml(p.name)}${p.has_key ? "" : " (нет ключа)"}</option>`
    )
    .join("");
  // The server's configured provider (wizard / CWI_AGENT_PROVIDER) is
  // authoritative — it must win over a stale localStorage choice so the
  // dropdown/badge match the running engine (and the per-message provider we
  // send). A UI switch still applies for that session and persists, but a
  // reload snaps back to the server's default. Fall back to saved → first-keyed.
  const activeOk = list.find((p) => p.id === state.activeProvider && p.has_key);
  const savedOk = list.find((p) => p.id === settings.provider && p.has_key);
  const firstKeyed = list.find((p) => p.has_key);
  settings.provider = activeOk
    ? state.activeProvider
    : savedOk
    ? settings.provider
    : firstKeyed
    ? firstKeyed.id
    : (list[0] && list[0].id) || "";
  el.provider.value = settings.provider;
  populateProviderModels();
}

export function populateProviderModels() {
  const p = state.providers.find((x) => x.id === settings.provider);
  const models = (p && p.models) || [];
  el.model.innerHTML = models
    .map((m) => `<option value="${escapeHtml(m.id)}">${escapeHtml(m.display_name || m.id)}</option>`)
    .join("");
  // For the server's configured provider, default to its configured model — the
  // live /models list mixes chat, image, video and audio models, so the first
  // entry (or a stale localStorage pick) can be a non-chat model like
  // "qwen-image-*" that 400s the Messages API. The operator's chosen model wins.
  if (
    settings.provider === state.activeProvider &&
    state.activeModel &&
    models.some((m) => m.id === state.activeModel)
  ) {
    settings.model = state.activeModel;
  }
  if ([...el.model.options].some((o) => o.value === settings.model)) {
    el.model.value = settings.model;
  } else if (el.model.options.length) {
    el.model.value = el.model.options[0].value;
    settings.model = el.model.value;
    localStorage.setItem("cwi_model", settings.model);
  }
  renderEngineBadge(); // keep the engine badge's provider/model in sync
}

el.provider.addEventListener("change", () => {
  settings.provider = el.provider.value;
  localStorage.setItem("cwi_provider", settings.provider);
  settings.model = ""; // reset → pick the new provider's first model
  populateProviderModels();
});

export function markActive(seg, attr, value) {
  seg.querySelectorAll(".seg-btn").forEach((b) => {
    b.classList.toggle("active", b.dataset[attr] === value);
  });
}

el.fontSeg.addEventListener("click", (e) => {
  const btn = e.target.closest(".seg-btn");
  if (!btn) return;
  settings.fontSize = btn.dataset.size;
  localStorage.setItem("cwi_fontsize", settings.fontSize);
  applySettings();
});

// While a drawer is open its badge is hidden (immediately); on close, the badge
// fades back in shortly after the panel has slid away.
export function hideBadge(badge) {
  badge.style.transition = "none";
  badge.style.opacity = "0";
  badge.style.pointerEvents = "none";
}
export function showBadgeSoon(badge) {
  setTimeout(() => {
    badge.style.transition = "opacity .3s ease";
    badge.style.opacity = "1";
    badge.style.pointerEvents = "";
  }, 500);
}

// Reload button in the title capsule. Reloading mid-turn is safe: the keeper
// process owns the session server-side and survives the socket dropping, so the
// answer keeps streaming and replays on reconnect.
el.reloadBtn.addEventListener("click", (e) => {
  e.stopPropagation(); // don't trigger the capsule's own hover/expand behaviour
  location.reload();
});

// Settings drawer (right) — open via the gear, close by clicking outside.
export function setSettings(open) {
  if (open) {
    setUsage(false); // only one right drawer at a time
    setChatActions(false);
    setPartyPanel(false);
  }
  el.settingsPanel.classList.toggle("open", open);
  el.settingsOverlay.hidden = !open;
  // Right badges ride the drawer's left edge (via `.open`) instead of hiding.
  el.settingsBadge.classList.toggle("open", open);
  el.usageBadge.classList.toggle("open", open);
  el.chatActionsBadge.classList.toggle("open", open);
  el.partyBadge.classList.toggle("open", open);
}
el.settingsBadge.addEventListener("click", () => setSettings(!el.settingsPanel.classList.contains("open")));
el.settingsOverlay.addEventListener("click", () => setSettings(false));

// Current-chat actions drawer (right, badge under the gear). Same width and
// slide as the settings drawer, so all three right panels behave identically.
export function setChatActions(open) {
  if (open && !state.sessionId) return; // nothing to act on without an open chat
  if (open) {
    setSettings(false); // only one right drawer at a time
    setUsage(false);
    setPartyPanel(false);
  }
  el.chatActionsPanel.classList.toggle("open", open);
  el.chatActionsOverlay.hidden = !open;
  el.chatActionsBadge.classList.toggle("open", open);
  el.settingsBadge.classList.toggle("open", open);
  el.usageBadge.classList.toggle("open", open);
  el.partyBadge.classList.toggle("open", open);
}
el.chatActionsBadge.addEventListener("click", () =>
  setChatActions(!el.chatActionsPanel.classList.contains("open")),
);
el.chatActionsOverlay.addEventListener("click", () => setChatActions(false));

// Party (room) drawer — right side, below the chat-actions badge. Same slide and
// mutual-exclusion as the other three right panels. The chat/control logic lives
// in party.js, reached via the `cwi-party-open` event; this owns open/close.
export function setPartyPanel(open) {
  if (open) {
    setSettings(false); // only one right drawer at a time
    setChatActions(false);
    setUsage(false);
  }
  el.partyPanel.classList.toggle("open", open);
  el.partyOverlay.hidden = !open;
  el.partyBadge.classList.toggle("open", open);
  el.settingsBadge.classList.toggle("open", open);
  el.usageBadge.classList.toggle("open", open);
  el.chatActionsBadge.classList.toggle("open", open);
  if (open) window.dispatchEvent(new CustomEvent("cwi-party-open"));
}
el.partyBadge.addEventListener("click", () =>
  setPartyPanel(!el.partyPanel.classList.contains("open")),
);
el.partyOverlay.addEventListener("click", () => setPartyPanel(false));

// Token badge → detailed per-chat usage drawer.
el.usageBadge.addEventListener("click", (e) => {
  // Chevron at the top → collapse to the dot. Dot (collapsed) → expand. Anywhere
  // else on the expanded badge → open the detailed usage drawer (as before).
  const setCollapsed = (v) => {
    state.usageCollapsed = v;
    try { localStorage.setItem("cwi_usage_collapsed", v ? "1" : "0"); } catch {}
    updateUsageBadge();
  };
  if (e.target.closest && e.target.closest(".ub-collapse")) return setCollapsed(true);
  if (el.usageBadge.classList.contains("collapsed")) return setCollapsed(false);
  setUsage(true);
});
el.usageOverlay.addEventListener("click", () => setUsage(false));

// All three left badges ride on the edge of whichever left drawer is open (they
// slide right with it via `.side-badge.open`) instead of being hidden — so none
// ever sits on top of the open panel. The files drawer is wider than the other
// two, hence its own offset class.
function updateLeftBadges() {
  const filesOpen = el.filesDrawer.classList.contains("open");
  const anyOpen =
    filesOpen ||
    el.sidebar.classList.contains("open") ||
    el.adminDrawer.classList.contains("open");
  for (const badge of [el.sidebarBadge, el.adminBadge, el.filesBadge]) {
    badge.classList.toggle("open", anyOpen);
    badge.classList.toggle("open-wide", filesOpen);
  }
}

// Chat list drawer (left) — mirrors the settings drawer.
export function setSidebar(open) {
  if (open) {
    // only one left drawer at a time
    setAdminDrawer(false);
    setFilesDrawer(false);
  }
  el.sidebar.classList.toggle("open", open);
  el.sidebarOverlay.hidden = !open;
  updateLeftBadges();
}
el.sidebarBadge.addEventListener("click", () => setSidebar(!el.sidebar.classList.contains("open")));
el.sidebarOverlay.addEventListener("click", () => setSidebar(false));

// Admin controls drawer (left, above the chats badge) — Гостевой сервер + Ссылки.
// The badge/drawer are hidden on a guest instance (see links.js admin gate), so
// these listeners simply never fire there. Opening it refreshes both panels via
// the `cwi-admin-open` event (guest.js requests VM status; links.js reloads).
export function setAdminDrawer(open) {
  if (open) {
    setSidebar(false); // only one left drawer at a time
    setFilesDrawer(false);
  }
  el.adminDrawer.classList.toggle("open", open);
  el.adminOverlay.hidden = !open;
  updateLeftBadges();
  if (open) window.dispatchEvent(new CustomEvent("cwi-admin-open"));
}
el.adminBadge.addEventListener("click", () => setAdminDrawer(!el.adminDrawer.classList.contains("open")));
el.adminOverlay.addEventListener("click", () => setAdminDrawer(false));

// Files drawer (left, topmost badge) — read-only workspace explorer. The listing
// itself lives in files.js, which listens for `cwi-files-open`; this only owns
// the drawer's open/close state, like the two above it.
export function setFilesDrawer(open) {
  if (open) {
    setSidebar(false); // only one left drawer at a time
    setAdminDrawer(false);
  }
  el.filesDrawer.classList.toggle("open", open);
  el.filesOverlay.hidden = !open;
  updateLeftBadges();
  if (open) window.dispatchEvent(new CustomEvent("cwi-files-open"));
}
el.filesBadge.addEventListener("click", () => setFilesDrawer(!el.filesDrawer.classList.contains("open")));
el.filesOverlay.addEventListener("click", () => setFilesDrawer(false));

// In-drawer collapse buttons: each `.drawer-close` names its drawer via
// `data-close`, so one map wires them all (mobile can't tap an outside overlay).
const DRAWER_CLOSERS = {
  settings: () => setSettings(false),
  "chat-actions": () => setChatActions(false),
  usage: () => setUsage(false),
  party: () => setPartyPanel(false),
  sidebar: () => setSidebar(false),
  admin: () => setAdminDrawer(false),
  files: () => setFilesDrawer(false),
};
for (const btn of document.querySelectorAll(".drawer-close")) {
  btn.addEventListener("click", () => DRAWER_CLOSERS[btn.dataset.close]?.());
}

// Composer + title show only when a chat is open. With no chat open, reveal the
// list; if there are no chats at all, offer a big "create" button instead.
export function refreshComposerState() {
  const open = !!state.sessionId;
  const chatsExist = el.chatList.children.length > 0;
  el.composer.style.display = open ? "" : "none";
  el.title.style.display = open ? "" : "none";
  // Chat actions only exist while a chat is open; closing one must also close
  // the drawer, or it would linger over an empty chat area.
  el.chatActionsBadge.hidden = !open;
  if (!open) setChatActions(false);
  el.bigNewChat.hidden = open || chatsExist;

  // Frozen chat (created by the other engine): read-only until CWI_ENGINE flips.
  // Hide the whole input row (textarea + left buttons + send); leave the banner.
  const frozen = open && chatFrozen(state.sessionId);
  el.composer.classList.toggle("readonly", frozen);
  el.input.disabled = frozen;
  el.input.placeholder = frozen
    ? "Только чтение — этот чат создан другим движком"
    : "Enter: send,  Shift+Enter: new line";
  if (el.frozenBanner) {
    el.frozenBanner.hidden = !frozen;
    if (frozen) {
      // CLI/subscription label matches the badge header: "Cloud <plan>" (e.g.
      // "Cloud Max"). Plan comes from the subscription usage when known.
      const cloudLabel = () => {
        const plan = state.usage && state.usage.plan ? String(state.usage.plan) : "";
        const nice = plan ? " " + plan.charAt(0).toUpperCase() + plan.slice(1).toLowerCase() : "";
        return "Cloud" + nice;
      };
      const modeName = (nat) => (nat ? "native (/v1/messages)" : cloudLabel());
      const chatMode = modeName(state.chatEngine[state.sessionId] === "native");
      const serverMode = modeName(state.engineNative === true);
      el.frozenBanner.innerHTML =
        iIcon("lock", 15, "frozen-lock") +
        `<div class="frozen-body">` +
        `<div>Сервер сейчас запущен в режиме «${escapeHtml(serverMode)}», а этот чат создан в «${escapeHtml(chatMode)}» — поэтому он доступен только для чтения. Выберите подходящий чат из списка или создайте новый.</div>` +
        `<div class="frozen-actions">` +
        `<button type="button" class="frozen-btn" data-act="new">${iIcon("plus", 14)} Новый чат</button>` +
        `<button type="button" class="frozen-btn ghost" data-act="list">${iIcon("menu", 14)} Список чатов</button>` +
        `</div></div>`;
      el.frozenBanner.querySelector('[data-act="new"]')
        ?.addEventListener("click", openNewChatModal);
      el.frozenBanner.querySelector('[data-act="list"]')
        ?.addEventListener("click", () => setSidebar(true));
    } else {
      el.frozenBanner.innerHTML = "";
    }
  }
  updateSendButton();
}
el.bigNewChat.addEventListener("click", openNewChatModal);

// ---------------------------------------------------------------------------
// Chat list / history
// ---------------------------------------------------------------------------
export async function loadChatList() {
  try {
    const res = await fetch("/api/chats");
    const chats = await res.json();
    renderChatList(chats);
  } catch (e) {
    // ignore
  }
}

export function renderChatList(chats) {
  el.chatList.innerHTML = "";
  state.chatUsage = {};
  for (const c of chats) {
    state.chatUsage[c.id] = {
      tokens: c.tokens || 0,
      input_tokens: c.input_tokens || 0,
      cache_read: c.cache_read || 0,
      cache_creation: c.cache_creation || 0,
      turns: c.turns || 0,
      duration_ms: c.duration_ms || 0,
      contextTokens: c.last_context_tokens || 0,
      contextLimit: c.context_limit || 0,
      models: c.models || [],
      model: c.model || "",
    };
    state.chatEngine[c.id] = c.engine || "cli";
    const item = document.createElement("div");
    item.className = "chat-item";
    item.dataset.id = c.id;
    item.dataset.title = c.title;
    item.dataset.icon = c.icon || "";
    item.dataset.engine = c.engine || "cli";

    const row = document.createElement("div");
    row.className = "chat-title-row";

    if (c.icon) {
      const icon = document.createElement("span");
      icon.className = "chat-icon";
      icon.innerHTML = iIcon(c.icon, 18, "inline");
      row.appendChild(icon);
    }

    const title = document.createElement("div");
    title.className = "chat-title-text";
    title.textContent = c.title;
    row.appendChild(title);

    if (c.id === state.sessionId) item.classList.add("active");
    item.appendChild(row);
    item.addEventListener("click", () => openChat(c.id, item.dataset.title, item.dataset.icon, item));
    el.chatList.appendChild(item);
  }
  redecorateChatList();
  refreshComposerState();
  updateUsageBadge();
  // Search box only makes sense with something to search: show it when there's
  // more than one chat; hide (and clear) it for 0 or 1 so a stale filter can't
  // hide the lone chat.
  if (el.chatSearch) {
    const many = chats.length > 1;
    el.chatSearch.hidden = !many;
    if (!many) el.chatSearch.value = "";
  }
  filterChats(); // re-apply any active search filter after a refresh
}

// Mark chats whose owning engine differs from the active one as frozen
// (dimmed + 🔒). Called after the list renders and again once the active engine
// resolves via /api/providers (which may land after the first render).
export function redecorateChatList() {
  const active =
    state.engineNative == null ? null : state.engineNative ? "native" : "cli";
  for (const item of el.chatList.children) {
    const owner = item.dataset.engine || "cli";
    const frozen = active != null && owner !== active;
    item.classList.toggle("frozen", frozen);
    let lock = item.querySelector(".chat-lock");
    if (frozen && !lock) {
      lock = document.createElement("span");
      lock.className = "chat-lock";
      lock.innerHTML = iIcon("lock", 14, "");
      lock.title = "Только чтение — чат создан другим движком";
      const row = item.querySelector(".chat-title-row");
      if (row) row.appendChild(lock);
    } else if (!frozen && lock) {
      lock.remove();
    }
  }
}

// ---------------------------------------------------------------------------
// Chat search — client-side filter over the already-rendered list by title.
// ---------------------------------------------------------------------------
export function filterChats() {
  const q = (el.chatSearch && el.chatSearch.value.trim().toLowerCase()) || "";
  for (const item of el.chatList.children) {
    const title = (item.dataset.title || "").toLowerCase();
    item.style.display = !q || title.includes(q) ? "" : "none";
  }
}
if (el.chatSearch) el.chatSearch.addEventListener("input", filterChats);

// ---------------------------------------------------------------------------
// Delete a chat (kills the live keeper + removes files server-side).
// ---------------------------------------------------------------------------
export async function deleteChat(id, title, item) {
  const confirmed = await confirmChatAction({
    title: "Удалить чат?",
    message: `Чат «${title}» и его история будут удалены без возможности восстановления.`,
    confirmLabel: "Удалить",
    danger: true,
  });
  if (!confirmed) return;
  try {
    const res = await fetch(`/api/chats/${encodeURIComponent(id)}`, { method: "DELETE" });
    if (!res.ok && res.status !== 204) throw new Error(res.status);
  } catch (e) {
    showSystem("Не удалось удалить чат.");
    return;
  }
  if (item) item.remove();
  else loadChatList(); // no DOM node passed (deleted from the chat view) → refresh list
  delete state.chatUsage[id];
  // If the open chat was the one deleted, clear the view and persisted state.
  if (state.sessionId === id) {
    // Deleting a chat kills its keeper server-side; if it was mid-stream, clear
    // the local streaming state too, or the composer stays locked until reload.
    if (state.streaming) {
      state.streaming = false;
      setStreamingUI(false);
      setFaviconState("idle");
    }
    state.sessionId = null;
    state.isNew = true;
    state.current = null;
    state.transcript = null; // don't let a later new chat re-render this history
    state.queue = [];
    renderQueue();
    el.titleName.textContent = "";
    resetMessages();
    el.messages.innerHTML =
      '<div class="empty-state"><h1>Agent Web</h1><p>Чат удалён. Выберите другой или создайте новый.</p></div>';
    setSidebar(true);
    try {
      localStorage.removeItem("cwi_last_chat");
      localStorage.removeItem("cwi_live_session");
    } catch (e) {}
  }
  refreshComposerState();
  updateUsageBadge();
}

// ---------------------------------------------------------------------------
// Export a chat's transcript as a Markdown file (built client-side).
// ---------------------------------------------------------------------------
export async function exportChat(id, title, icon) {
  let msgs;
  try {
    msgs = await (await fetch(`/api/chats/${encodeURIComponent(id)}`)).json();
  } catch (e) {
    showSystem("Не удалось загрузить чат для экспорта.");
    return;
  }
  downloadFile(transcriptToMarkdown(msgs, title, icon), safeFilename(title || id) + ".md", "text/markdown");
}

// Export a chat's full transcript as pretty-printed JSON (roles, text, tools).
export async function exportChatJson(id, title) {
  let msgs;
  try {
    msgs = await (await fetch(`/api/chats/${encodeURIComponent(id)}`)).json();
  } catch (e) {
    showSystem("Не удалось загрузить чат для экспорта.");
    return;
  }
  const doc = { id, title, exported_at: new Date().toISOString(), messages: msgs };
  downloadFile(JSON.stringify(doc, null, 2), safeFilename(title || id) + ".json", "application/json");
}

// Trigger a client-side download of `text` as a file.
function downloadFile(text, filename, mime) {
  const blob = new Blob([text], { type: `${mime};charset=utf-8` });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  a.remove();
  URL.revokeObjectURL(url);
}

// Build a Markdown document from a loaded transcript. Tool calls are rendered
// as fenced blocks so the export is self-contained and readable.
export function transcriptToMarkdown(msgs, title, icon) {
  const lines = [`# ${title || "Чат"}`, ""];
  for (const m of msgs) {
    if (m.role === "user") {
      lines.push("## Пользователь", "");
      if (m.text) lines.push(m.text, "");
    } else {
      lines.push("## Ассистент", "");
      if (m.text) lines.push(m.text, "");
      for (const t of m.tools || []) {
        lines.push(`**Инструмент: ${t.name}**`, "", "```json", JSON.stringify(t.input || {}, null, 2), "```", "");
      }
    }
  }
  return lines.join("\n");
}

function safeFilename(s) {
  return s.replace(/[\\/:*?"<>|]+/g, "_").replace(/\s+/g, "_").slice(0, 80) || "chat";
}

// Re-trigger the input-row entrance animation (remove class, force reflow, re-add).
function playComposerIn() {
  const row = el.composerRow;
  if (!row) return;
  row.classList.remove("slide-in");
  void row.offsetWidth; // reflow so the animation restarts on every open
  row.classList.add("slide-in");
}

export async function openChat(id, title, icon, item) {
  if (state.streaming) return;
  setSidebar(false); // close the drawer once a chat is picked
  state.sessionId = id;
  // Remember this chat so it reopens automatically after a page reload.
  try {
    localStorage.setItem("cwi_last_chat", id);
    localStorage.setItem("cwi_live_session", JSON.stringify({ id, ts: Date.now() }));
  } catch (e) {}
  state.isNew = false; // existing chat -> resume on next turn
  state.current = null;
  setFaviconState("idle");
  el.titleName.innerHTML = (icon ? iIcon(icon, 18, "inline") + "  " : "") + escapeHtml(title);
  document.querySelectorAll(".chat-item.active").forEach((n) => n.classList.remove("active"));
  if (item) item.classList.add("active");
  updateUsageBadge(); // reflect the newly-opened chat's token total

  // Decide read-only vs active BEFORE the async transcript load, so the composer
  // never flashes the input row on a frozen chat (chatFrozen is synchronous).
  // For an active chat, slide the input row in smoothly.
  refreshComposerState();
  if (!chatFrozen(id)) playComposerIn();

  el.messages.innerHTML = "";
  state.transcript = null;
  state.queue = []; // queued messages belong to the chat you left
  renderQueue();
  try {
    const res = await fetch(`/api/chats/${encodeURIComponent(id)}`);
    const msgs = await res.json();
    if (!msgs.length) {
      showSystem("В этом чате пока нет сообщений.");
      // Empty/placeholder chat (never had a real turn) → start a fresh session on
      // the first send instead of trying to --resume a non-existent conversation.
      state.isNew = true;
    }
    // Windowed render: only the last MAX_RENDERED messages are in the DOM; a
    // "load earlier" button reveals older ones. Bounds the DOM for huge chats.
    const start = Math.max(0, msgs.length - MAX_RENDERED);
    state.transcript = { msgs, start };
    const frag = document.createDocumentFragment();
    renderMsgRange(msgs, start, msgs.length, frag);
    el.messages.appendChild(frag);
    updateEarlierButton();
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

const MAX_RENDERED = 60; // messages rendered initially when opening a chat
const LOAD_CHUNK = 40; // older messages revealed per "load earlier" click

// Render a transcript with windowing: only the last MAX_RENDERED messages go in
// the DOM, with a "load earlier" button for the rest, and `state.transcript.start`
// is set so paging works. Used both on open and on reattach (replay_end), so the
// DOM stays bounded either way (a full re-render on reconnect used to blow it up).
export function renderTranscriptWindowed(msgs) {
  resetMessages();
  const start = Math.max(0, msgs.length - MAX_RENDERED);
  state.transcript = { msgs, start };
  const frag = document.createDocumentFragment();
  renderMsgRange(msgs, start, msgs.length, frag);
  el.messages.appendChild(frag);
  updateEarlierButton();
  scrollToBottom();
}

export function updateEarlierButton() {
  let btn = document.getElementById("load-earlier");
  const start = state.transcript ? state.transcript.start : 0;
  if (!state.transcript || start <= 0) {
    if (btn) btn.remove();
    return;
  }
  if (!btn) {
    btn = document.createElement("button");
    btn.id = "load-earlier";
    btn.className = "load-earlier";
    btn.type = "button";
    btn.addEventListener("click", loadEarlier);
  }
  btn.textContent = `↑ Показать более ранние (${start})`;
  el.messages.insertBefore(btn, el.messages.firstChild); // keep at the very top
}

export function loadEarlier() {
  const t = state.transcript;
  if (!t || t.start <= 0) return;
  const newStart = Math.max(0, t.start - LOAD_CHUNK);
  const before = el.messages.scrollHeight;
  const prevTop = el.messages.scrollTop;
  const frag = document.createDocumentFragment();
  renderMsgRange(t.msgs, newStart, t.start, frag);
  const btn = document.getElementById("load-earlier");
  const anchor = btn ? btn.nextSibling : el.messages.firstChild;
  el.messages.insertBefore(frag, anchor);
  t.start = newStart;
  updateEarlierButton();
  // Keep the viewport steady: prepended content shifts everything down.
  el.messages.scrollTop = prevTop + (el.messages.scrollHeight - before);
}

export function fmtDate(iso) {
  try {
    const d = new Date(iso);
    return d.toLocaleDateString() + " " + d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  } catch {
    return "";
  }
}

// ---------------------------------------------------------------------------
// Init — called by main.js after all modules have loaded.
// ---------------------------------------------------------------------------
// Fade out the boot overlay and reveal the UI. Idempotent — the first caller
// wins (either the normal boot path or the safety timeout).
let revealed = false;
function reveal() {
  if (revealed) return;
  revealed = true;
  document.body.classList.add("booted"); // triggers the staggered CSS reveal
  const boot = document.getElementById("boot");
  if (boot) {
    boot.classList.add("hide");
    setTimeout(() => boot.remove(), 450);
  }
}

// Guest access countdown: fetch when this session's access expires and tick it
// live as the second line of the chat-title capsule. Owner instances (gate off)
// report gated:false and show nothing.
async function loadSessionTimer() {
  let info;
  try {
    const r = await fetch("/api/session");
    if (!r.ok) return;
    info = await r.json();
  } catch (e) {
    return;
  }
  if (!info || !info.gated) return; // owner → no countdown, no seat keep-alive
  // Guest instance: party roles apply, and we prompt for a room name.
  state.gated = true;
  window.dispatchEvent(new CustomEvent("cwi-gated"));
  // Access-expiry countdown (second line of the title capsule).
  if (info.expires != null) {
    const expiresAtMs = info.expires * 1000;
    const tick = () => {
      el.sessionTimer.textContent = fmtRemaining(expiresAtMs - Date.now());
      el.sessionTimer.hidden = false;
    };
    tick();
    setInterval(tick, 1000);
  }
  // Notify the guest when the host starts a Drain-Stop (server shutting down).
  watchDrain();
}

// Poll health; when the host flips this instance into drain, show a one-time
// centered notice (reusing the keep-alive card) so the guest knows the current
// answer will finish, no new messages are taken, and they can leave.
function watchDrain() {
  el.drainNotice.addEventListener("click", () => { el.drainNotice.hidden = true; });
  const check = () => {
    fetch("/api/health")
      .then((r) => (r.ok ? r.json() : null))
      .then((h) => {
        if (!h) return;
        const draining = !!h.draining;
        // Show the notice once, on the transition into drain.
        if (draining && !state.draining) el.drainNotice.hidden = false;
        // Keep the flag live so submit()/flushQueue()/the send button all know
        // the server is winding down and stop feeding the queue.
        state.draining = draining;
        updateSendButton();
      })
      .catch(() => {});
  };
  check();
  setInterval(check, 15000);
}

// Single-seat keep-alive removed: a magic link now admits many people (the room
// model in party.js), so there's no idle-seat warning or eviction. Access still
// ends when the cookie/code expires (the countdown above); driver hand-off idle
// is tracked per session on the server, not by a page-activity ping.

function fmtRemaining(ms) {
  if (ms <= 0) return "доступ истёк";
  const s = Math.floor(ms / 1000);
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  let body;
  if (d > 0) body = `${d}д ${h}ч`;
  else if (h > 0) body = `${h}ч ${m}м`;
  else if (m > 0) body = `${m}м ${sec}с`;
  else body = `${sec}с`;
  return `истекает через ${body}`;
}

export async function init() {
  applySettings();
  connect();
  updateScrollbar();
  updateScrollToBottomButton();
  // Persist the open chat before the page is frozen/reloaded so we can re-attach
  // to its live session when the app comes back up.
  window.addEventListener("pagehide", saveLiveSession);
  window.addEventListener("beforeunload", saveLiveSession);
  // Safety net: never leave the app hidden behind the boot overlay if a fetch hangs.
  setTimeout(reveal, 6000);

  // Resolve EVERYTHING behind the boot overlay — chat list, active engine, and
  // the last-open chat's transcript — then reveal the finished UI in one pass, so
  // it never flashes the empty "+ new chat" page or switches mid-load.
  try {
    // Resolve the active engine FIRST (sets state.engineNative), THEN render the
    // chat list — so cross-engine chats are marked read-only (🔒) from the very
    // first paint. Loading them in parallel left a window where an incompatible
    // chat rendered as normal (no lock) and could be acted on before the engine
    // resolved.
    await loadProviders();
    await loadChatList();
    await restoreLastChat();
  } catch (e) {}
  refreshComposerState();
  if (!state.sessionId) setSidebar(true); // no chat restored → reveal the list
  reveal();
  loadUsage(); // subscription limits for the badge + sidebar (refreshed rarely)
  loadSessionTimer(); // guest-only: countdown to access expiry in the title capsule
  el.input.focus();
  // The chat list is loaded on page load and refreshed when a chat is created
  // (see createNewChat) — no periodic polling.
}

function saveLiveSession() {
  if (!state.sessionId) return;
  try {
    localStorage.setItem("cwi_last_chat", state.sessionId);
    localStorage.setItem("cwi_live_session", JSON.stringify({ id: state.sessionId, ts: Date.now() }));
  } catch (e) {}
}

// If the user had a chat open during the previous session, reopen it once the
// sidebar has loaded so it is visible and correctly marked active.
export function restoreLastChat() {
  let lastId = null;
  try { lastId = localStorage.getItem("cwi_last_chat"); } catch (e) {}
  if (!lastId) return;
  const item = el.chatList.querySelector(`.chat-item[data-id="${CSS.escape(lastId)}"]`);
  if (item) {
    // Return the promise so boot can await the transcript before revealing the UI.
    return openChat(item.dataset.id, item.dataset.title, item.dataset.icon || null, item);
  } else {
    // The remembered chat no longer exists; clear stale persistence keys.
    try {
      localStorage.removeItem("cwi_last_chat");
      localStorage.removeItem("cwi_live_session");
    } catch (e) {}
  }
}
