import { state, el, escapeHtml, renderMarkdown, chatFrozen } from './state.js';
import { iIcon } from './ios-icons.js';
import { hideBadge, showBadgeSoon, loadChatList, updateEarlierButton, settings } from './ui.js';
import { setFaviconState } from '../favicon.js';
import { playCompletionChime } from '../sound.js';
import { notifyTurnComplete } from '../notify.js';
import { sendWs } from './ws.js';
// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------
// Render msgs[from..to) as grouped assistant/user blocks into `target`.
// Consecutive assistant messages merge into one visual response.
export function renderMsgRange(msgs, from, to, target) {
  let groupAnswer = null;
  const closeGroup = () => {
    if (groupAnswer) { addFoldIfLong(groupAnswer); groupAnswer = null; }
  };
  for (let i = from; i < to; i++) {
    const m = msgs[i];
    if (m.role === "user") {
      closeGroup();
      addUserMessage(m.text, null, target);
      continue;
    }
    if (!groupAnswer) {
      const { bodyEl } = makeMessage("assistant", target);
      groupAnswer = document.createElement("div");
      groupAnswer.className = "answer";
      bodyEl.appendChild(groupAnswer);
    }
    if (m.text) {
      const content = document.createElement("div");
      content.className = "content";
      content.innerHTML = renderMarkdown(m.text);
      groupAnswer.appendChild(content);
    }
    (m.tools || []).forEach((t) => renderToolCard({ answerEl: groupAnswer }, t.name, t.input || {}));
  }
  closeGroup();
}

export function ensureAssistant() {
  if (state.current) return state.current;
  clearEmptyState();
  const { msgEl, bodyEl } = makeMessage("assistant");
  msgEl.classList.add("streaming");
  const answerEl = document.createElement("div");
  answerEl.className = "answer";
  bodyEl.appendChild(answerEl);
  const statusEl = document.createElement("div");
  statusEl.className = "stream-status";
  const statusDetailEl = document.createElement("div");
  statusDetailEl.className = "status-detail";
  statusDetailEl.hidden = true;
  bodyEl.appendChild(statusEl);
  bodyEl.appendChild(statusDetailEl);
  const cur = {
    msgEl, bodyEl, answerEl, statusEl, statusDetailEl,
    textEl: null, textRaw: "",
    thinkStart: null, thinkEnd: null, thinkMs: null, thinkTokens: 0, thinkChars: 0, thinkRaw: "",
    startTime: Date.now(), tokens: 0, runningTasks: 0, status: "печатает…",
  };
  state.current = cur;
  // Toggle the reasoning detail panel; only active once there's a reasoning
  // phase to show (the "has-detail" class, set in `updateStatus`/
  // `finalizeStatusLine`, gates this so clicking an ordinary line is a no-op).
  // Closes over THIS turn's own `cur` — not `state.current` — so a finalized,
  // historical message's line still expands its own data after later turns
  // have moved `state.current` on.
  statusEl.addEventListener("click", () => {
    if (!statusEl.classList.contains("has-detail")) return;
    const opening = statusDetailEl.hidden;
    statusDetailEl.hidden = !opening;
    statusEl.classList.toggle("expanded", opening);
    if (opening) renderStatusDetail(cur);
  });
  state.streaming = true;
  setStreamingUI(true);
  setFaviconState("thinking");
  if (state.statusTimer) clearInterval(state.statusTimer);
  state.statusTimer = setInterval(updateStatus, 1000); // live elapsed timer
  updateStatus();
  // Reveal the new bubble immediately — otherwise it lands behind the fixed
  // composer and only scrolls into view once later content forces a re-scroll.
  scrollToBottomIfPinned();
  return state.current;
}

// Live status line during generation: elapsed · tokens · running tasks · phrase.
// Gains a trailing chevron once a reasoning phase starts (see `ensureThinking`)
// — expanding it reveals the duration/token estimate (and raw text, native
// engine only) that used to live in a separate always-visible "thinking" box.
export function updateStatus() {
  const c = state.current;
  if (!c) return;
  if (!c.statusEl) return;
  const parts = [fmtElapsed(Date.now() - c.startTime), `${c.tokens} токенов`];
  if (c.runningTasks > 0) parts.push(`${c.runningTasks} ${c.runningTasks === 1 ? "задача" : "задач"}`);
  parts.push(c.status);
  const hasDetail = c.thinkStart != null;
  c.statusEl.classList.toggle("has-detail", hasDetail);
  c.statusEl.innerHTML =
    `<span class="pulse"></span><span class="status-text">${escapeHtml(parts.join(" · "))}</span>` +
    (hasDetail ? `<span class="status-chevron">${CHEVRON_SVG}</span>` : "");
  if (hasDetail && c.statusDetailEl && !c.statusDetailEl.hidden) renderStatusDetail(c);
}
export function fmtElapsed(ms) {
  const s = Math.floor(ms / 1000);
  return s < 60 ? `${s}с` : `${Math.floor(s / 60)}м ${s % 60}с`;
}

export function appendText(cur, text) {
  stopThinkingClock(cur); // the answer is starting → stop counting reasoning time
  if (!cur.textEl) {
    cur.textEl = document.createElement("div");
    cur.textEl.className = "content cursor";
    cur.answerEl.appendChild(cur.textEl);
    cur.textRaw = "";
  }
  cur.textRaw += text;
  cur.textEl.innerHTML = renderMarkdown(cur.textRaw);
  // Follow the stream while the user is near the bottom (state.followBottom is
  // driven by their scrolling, so appends don't skew it like a post-hoc measure).
  scrollToBottomIfPinned();
}

// Chevron icon (points down); rotated 180° via CSS when expanded.
export const CHEVRON_SVG =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m6 9 6 6 6-6"/></svg>';

// Make `contentEl` collapse/expand (animated) via `chevronEl`. `rootEl` carries
// the `collapsed` state class (CSS anchors a collapsed answer to its last lines).
export function makeCollapsible(rootEl, contentEl, chevronEl) {
  rootEl.classList.add("cfold");
  contentEl.classList.add("fold-content");
  let collapsed = false;

  const collapsePx = () => {
    const lh = parseFloat(getComputedStyle(contentEl).lineHeight) || 22;
    return Math.round(lh * 2 + 12); // ~2 lines + headroom so the top one stays readable
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
    // Drop the collapsed layout FIRST: while collapsed the content is a flex
    // column anchored to the bottom, so overflow spills past the TOP edge and
    // scrollHeight (which ignores content above the box) reads as ~2 lines.
    // In normal block flow scrollHeight reports the true full height.
    rootEl.classList.remove("collapsed");
    const full = contentEl.scrollHeight; // measured with maxHeight still clamped
    contentEl.style.maxHeight = full + "px"; // animate up from the clamped height
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

// Mark the start of a reasoning phase. No dedicated DOM — folded into the
// status line's expandable detail (see `renderStatusDetail`): the Claude Code
// CLI never delivers actual reasoning text (only a token estimate), so a
// separate always-visible panel had nothing to show but a clock there anyway.
export function ensureThinking(cur) {
  if (cur.thinkStart != null) return;
  cur.thinkStart = Date.now();
  cur.thinkEnd = null; // set when the answer starts → the clock freezes
  updateStatus(); // reveal the detail chevron immediately, not on the next tick
}

// Accumulate reasoning text for the detail panel (native engine only — the
// CLI's estimate-only `thinking_delta` never sets this).
export function appendThinkingText(cur, text) {
  cur.thinkRaw = (cur.thinkRaw || "") + text;
  if (cur.statusDetailEl && !cur.statusDetailEl.hidden) renderStatusDetail(cur);
}

// The status line's expandable detail: reasoning duration + token estimate,
// plus the raw text when the engine delivers it (native mode only).
function renderStatusDetail(cur) {
  if (!cur.statusDetailEl || cur.thinkStart == null) return;
  // Prefer the authoritative duration from the engine (survives replay).
  const ms = cur.thinkMs != null ? cur.thinkMs : (cur.thinkEnd || Date.now()) - cur.thinkStart;
  const parts = [`Рассуждения: ${fmtElapsed(ms)}`];
  if (cur.thinkTokens) parts.push(`~${cur.thinkTokens} токенов`);
  let html = `<div class="status-detail-line">${escapeHtml(parts.join(" · "))}</div>`;
  if (cur.thinkRaw) html += `<div class="status-detail-text">${escapeHtml(cur.thinkRaw)}</div>`;
  cur.statusDetailEl.innerHTML = html;
}

// Freeze the reasoning clock (called when the answer begins or the turn ends).
export function stopThinkingClock(cur) {
  if (cur && cur.thinkStart != null && cur.thinkEnd == null) {
    cur.thinkEnd = Date.now();
  }
}

export function setThinkingTokens(cur, n) {
  if (n > cur.thinkTokens) cur.thinkTokens = n;
}

// Render every tool_use block of an assistant message as a card showing the
// tool name and its parameters (and a git-style +/- summary for file edits).
export function renderToolCalls(cur, evt) {
  const content = evt.message && evt.message.content;
  if (!Array.isArray(content)) return;
  for (const block of content) {
    if (block.type === "tool_use") {
      renderToolCard(cur, block.name || "tool", block.input || {});
    }
  }
}

export const TOOL_LINE_CAP = 40; // max code/diff lines shown per tool card

export function nLines(s) { return s ? String(s).split("\n").length : 0; }

export function shortPath(p) {
  if (!p) return "";
  const parts = String(p).split(/[\\/]/).filter(Boolean);
  return parts.length <= 2 ? p : "…/" + parts.slice(-2).join("/");
}

export function toolCode(container, text) {
  const pre = document.createElement("pre");
  pre.className = "tool-code";
  let lines = String(text).split("\n");
  const extra = Math.max(0, lines.length - TOOL_LINE_CAP);
  if (extra) lines = lines.slice(0, TOOL_LINE_CAP);
  pre.textContent = lines.join("\n");
  container.appendChild(pre);
  if (extra) toolMore(container, extra);
}

// Interleaved (unified) line diff via a simple LCS — context lines kept, changed
// lines marked. `no` is the line number on the MODIFIED side (blank for deletions).
// Numbers are relative to the snippet: an Edit carries no absolute file positions.
export function lineDiff(oldStr, newStr) {
  const a = String(oldStr || "").split("\n");
  const b = String(newStr || "").split("\n");
  const n = a.length, m = b.length;
  const dp = Array.from({ length: n + 1 }, () => new Array(m + 1).fill(0));
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      dp[i][j] = a[i] === b[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
    }
  }
  const rows = [];
  let i = 0, j = 0;
  while (i < n && j < m) {
    if (a[i] === b[j]) { rows.push({ t: "ctx", text: a[i], no: j + 1 }); i++; j++; }
    else if (dp[i + 1][j] >= dp[i][j + 1]) { rows.push({ t: "del", text: a[i], no: null }); i++; }
    else { rows.push({ t: "add", text: b[j], no: j + 1 }); j++; }
  }
  while (i < n) { rows.push({ t: "del", text: a[i], no: null }); i++; }
  while (j < m) { rows.push({ t: "add", text: b[j], no: j + 1 }); j++; }
  return rows;
}

export function toolDiff(container, oldStr, newStr) {
  const rows = lineDiff(oldStr, newStr);
  const extra = Math.max(0, rows.length - TOOL_LINE_CAP);
  const show = extra ? rows.slice(0, TOOL_LINE_CAP) : rows;
  const box = document.createElement("div");
  box.className = "tool-diff";
  box.innerHTML = show
    .map((r) => {
      const sign = r.t === "add" ? "+" : r.t === "del" ? "−" : " ";
      return (
        `<div class="d-row d-${r.t}">` +
        `<span class="d-sign">${sign}</span>` +
        `<span class="d-txt">${escapeHtml(r.text)}</span></div>`
      );
    })
    .join("");
  container.appendChild(box);
  if (extra) toolMore(container, extra);
}

export function toolMore(container, n) {
  const note = document.createElement("div");
  note.className = "tool-more";
  note.textContent = `… ещё ${n} строк`;
  container.appendChild(note);
}

export function renderToolCard(cur, name, input) {
  const card = document.createElement("div");
  card.className = "tool-call";

  const head = document.createElement("div");
  head.className = "tool-head";
  const nameEl = document.createElement("span");
  nameEl.className = "tool-name";
  nameEl.innerHTML = `${iIcon('gear', 14, 'inline')} ${escapeHtml(name)}`;
  const meta = document.createElement("span");
  meta.className = "tool-meta";
  head.appendChild(nameEl);
  head.appendChild(meta);

  const body = document.createElement("div");
  body.className = "tool-body";

  card.appendChild(head);
  card.appendChild(body);
  cur.answerEl.appendChild(card);

  renderToolBody({ name, meta, body }, input || {});

  // Collapsed by default — the user expands only what interests them. Make it
  // collapsible only when there's a body worth hiding.
  if (body.childNodes.length) {
    card.classList.add("tool-collapsible", "collapsed");
    const chev = document.createElement("span");
    chev.className = "tool-chevron";
    chev.innerHTML = CHEVRON_SVG;
    head.appendChild(chev);
    head.addEventListener("click", () => card.classList.toggle("collapsed"));
  }
  // Same single source of truth as the rest (state.followBottom via
  // scrollToBottomIfPinned) — no separate post-append measurement that could
  // disagree if the user is scrolling away just as the card appears.
  scrollToBottomIfPinned();
}

export function renderToolBody(t, input) {
  const name = t.name;
  const path = input.file_path || input.path || input.notebook_path;

  if (name === "Bash") {
    if (input.description) {
      const d = document.createElement("div");
      d.className = "tool-desc";
      d.textContent = input.description;
      t.body.appendChild(d);
    }
    toolCode(t.body, input.command || "");
    return;
  }
  if (name === "Edit" || name === "MultiEdit") {
    const edits = name === "MultiEdit"
      ? input.edits || []
      : [{ old_string: input.old_string, new_string: input.new_string }];
    let added = 0, removed = 0;
    edits.forEach((e) => {
      const rows = lineDiff(e.old_string, e.new_string);
      rows.forEach((r) => { if (r.t === "add") added++; else if (r.t === "del") removed++; });
      toolDiff(t.body, e.old_string, e.new_string);
    });
    const addTag = added ? `<span class="diff-add">+${added}</span>` : "";
    const delTag = removed ? `<span class="diff-del">−${removed}</span>` : "";
    t.meta.innerHTML =
      `${escapeHtml(shortPath(path || ""))} ${addTag} ${delTag}`.trim();
    return;
  }
  if (name === "Write") {
    t.meta.innerHTML =
      `${escapeHtml(shortPath(path || ""))} <span class="diff-add">+${nLines(input.content)}</span>`;
    return;
  }
  if (name === "Grep" || name === "Glob") {
    if (input.path) t.meta.textContent = shortPath(input.path);
    if (input.pattern) toolCode(t.body, input.pattern);
    return;
  }
  if (path) {
    t.meta.textContent = shortPath(path); // Read, etc. — just the file
    return;
  }
  // Anything else: show each parameter. String values are printed raw so their
  // real newlines render (instead of escaped "\n" from JSON.stringify).
  const entries = Object.entries(input);
  if (entries.length) {
    const text = entries
      .map(([k, v]) => `${k}: ${typeof v === "string" ? v : JSON.stringify(v)}`)
      .join("\n\n");
    toolCode(t.body, text);
  }
}

// ---------------------------------------------------------------------------
// Tool approval / AskUserQuestion prompts (the CLI's `control_request` relayed
// over `cwi:"permission_request"` — see session.rs's `run_actor`). A card per
// request, live in the answer stream; resolved only by the server's own
// "permission_resolved" echo (same pattern as the user's own message bubble,
// which also renders only from its echo) — never finalized optimistically, so
// every viewer (and a page reload) ends up showing the same real outcome.
const pendingPermissionCards = new Map(); // request_id -> card element

export function renderPermissionRequest(evt) {
  const cur = ensureAssistant();
  const isQuestion = evt.tool_name === "AskUserQuestion";

  const card = document.createElement("div");
  card.className = "perm-card";

  const head = document.createElement("div");
  head.className = "perm-head";
  head.innerHTML = `${iIcon(isQuestion ? "help" : "shield", 15, "inline")} <span>${
    isQuestion ? "Уточняющий вопрос" : `Разрешить «${escapeHtml(evt.tool_name)}»?`
  }</span>`;
  card.appendChild(head);

  const body = document.createElement("div");
  body.className = "perm-body";
  card.appendChild(body);

  const submit = (allow, answers, response) => {
    if (card.classList.contains("pending-response") || !pendingPermissionCards.has(evt.request_id)) return;
    card.classList.add("pending-response"); // guard against double-click before the echo lands
    sendWs({
      type: "permission_response", request_id: evt.request_id, allow,
      answers: answers || undefined, response: response || undefined,
    });
  };

  if (isQuestion) {
    buildQuestionBody(body, (evt.input && evt.input.questions) || [], submit);
  } else {
    buildToolApprovalBody(body, evt.input, submit);
  }

  cur.answerEl.appendChild(card);
  pendingPermissionCards.set(evt.request_id, card);
  scrollToBottomIfPinned();
}

function buildToolApprovalBody(body, input, submit) {
  const pre = document.createElement("pre");
  pre.className = "tool-code";
  pre.textContent = JSON.stringify(input || {}, null, 2);
  body.appendChild(pre);

  const actions = document.createElement("div");
  actions.className = "perm-actions";
  const allowBtn = document.createElement("button");
  allowBtn.type = "button";
  allowBtn.className = "perm-btn perm-allow";
  allowBtn.textContent = "Разрешить";
  allowBtn.addEventListener("click", () => submit(true, null, null));
  const denyBtn = document.createElement("button");
  denyBtn.type = "button";
  denyBtn.className = "perm-btn perm-deny";
  denyBtn.textContent = "Отклонить";
  denyBtn.addEventListener("click", () => submit(false, null, null));
  actions.appendChild(allowBtn);
  actions.appendChild(denyBtn);
  body.appendChild(actions);
}

// Structured options vs. the freeform alternative are mutually exclusive at
// the whole-card level (the protocol's `response` field replaces ALL answers,
// not just one question's — see `Cmd::PermissionResponse` on the Rust side).
// Typing in the alternative dims every option; picking an option dims the
// alternative back — but neither is ever truly `disabled`, so the user can
// freely change their mind in either direction (per the user's own spec).
function buildQuestionBody(body, questions, submit) {
  const selected = questions.map(() => new Set()); // chosen option indices, per question
  const optionButtons = [];
  // Whichever the user touched most recently — drives which side is dimmed.
  // Neither side's data is ever cleared on switch, so going back restores it.
  let activeMode = null; // "options" | "freeform" | null

  questions.forEach((q, qi) => {
    const block = document.createElement("div");
    block.className = "perm-question-block";
    const qText = document.createElement("div");
    qText.className = "perm-question-text";
    qText.textContent = q.question || "";
    block.appendChild(qText);

    const opts = document.createElement("div");
    opts.className = "perm-options";
    (q.options || []).forEach((opt, oi) => {
      const optBtn = document.createElement("button");
      optBtn.type = "button";
      optBtn.className = "perm-option";
      optBtn.innerHTML = `<span class="perm-option-label">${escapeHtml(opt.label || "")}</span>` +
        (opt.description ? `<span class="perm-option-desc">${escapeHtml(opt.description)}</span>` : "");
      optBtn.addEventListener("click", () => {
        if (q.multiSelect) {
          optBtn.classList.toggle("selected");
          if (optBtn.classList.contains("selected")) selected[qi].add(oi);
          else selected[qi].delete(oi);
        } else {
          opts.querySelectorAll(".perm-option").forEach((b) => b.classList.remove("selected"));
          optBtn.classList.add("selected");
          selected[qi] = new Set([oi]);
        }
        // Picking an option makes it the active mode — but the alternative's
        // draft text is kept, not erased, so the user can still come back to it.
        activeMode = "options";
        syncModeVisuals();
      });
      optionButtons.push(optBtn);
      opts.appendChild(optBtn);
    });
    block.appendChild(opts);
    body.appendChild(block);
  });

  const altWrap = document.createElement("div");
  altWrap.className = "perm-alt";
  const altLabel = document.createElement("div");
  altLabel.className = "perm-alt-label";
  altLabel.textContent = "Ни один вариант не подходит — опишите свой:";
  const altInput = document.createElement("textarea");
  altInput.className = "perm-alt-input";
  altInput.rows = 3;
  altInput.placeholder = "Опишите альтернативный или гибридный вариант…";
  altWrap.appendChild(altLabel);
  altWrap.appendChild(altInput);
  body.appendChild(altWrap);

  const actions = document.createElement("div");
  actions.className = "perm-actions";
  const submitBtn = document.createElement("button");
  submitBtn.type = "button";
  submitBtn.className = "perm-btn perm-allow";
  submitBtn.textContent = "Отправить";
  actions.appendChild(submitBtn);
  body.appendChild(actions);

  function syncModeVisuals() {
    optionButtons.forEach((btn) => btn.classList.toggle("perm-option-dimmed", activeMode === "freeform"));
    altWrap.classList.toggle("perm-alt-dimmed", activeMode === "options");
    const hasFreeformText = altInput.value.trim().length > 0;
    const allAnswered = questions.length > 0 && selected.every((s) => s.size > 0);
    submitBtn.disabled = !(
      (activeMode === "freeform" && hasFreeformText) ||
      (activeMode === "options" && allAnswered)
    );
  }
  altInput.addEventListener("input", () => {
    // Typing takes over as the active mode; clearing the box falls back to
    // "options" if something's picked there, or to neutral if nothing is.
    activeMode = altInput.value.trim()
      ? "freeform"
      : selected.some((s) => s.size > 0) ? "options" : null;
    syncModeVisuals();
  });
  altInput.addEventListener("focus", () => {
    // Focus alone (no typing needed) revives a non-empty draft — it was
    // dimmed by picking an option, not cleared, so it's still "the answer"
    // the moment the user's attention returns to it.
    if (altInput.value.trim()) {
      activeMode = "freeform";
      syncModeVisuals();
    }
  });
  syncModeVisuals(); // starts disabled: nothing picked, nothing typed

  submitBtn.addEventListener("click", () => {
    // Respect whichever mode is ACTIVE, not just "does the textarea have
    // leftover text" — the other side's draft is kept but must not win.
    if (activeMode === "freeform") {
      submit(true, null, altInput.value.trim());
      return;
    }
    const answers = {};
    questions.forEach((q, qi) => {
      const labels = [...selected[qi]].map((oi) => q.options[oi] && q.options[oi].label).filter(Boolean);
      answers[q.question] = q.multiSelect ? labels : (labels[0] || "");
    });
    submit(true, answers, null);
  });
}

// Called from ws.js on the server's "permission_resolved" echo — the single
// source of truth for how a request ended, whoever actually answered it.
export function markPermissionResolved(requestId, allow, answers, response) {
  const card = pendingPermissionCards.get(requestId);
  if (!card) return;
  pendingPermissionCards.delete(requestId);
  card.classList.remove("pending-response");
  card.classList.add("resolved");
  const body = card.querySelector(".perm-body");
  if (!body) return;
  if (allow && response) {
    body.innerHTML = `<div class="perm-answer"><b>Ваш вариант:</b> ${escapeHtml(response)}</div>`;
  } else if (allow && answers && Object.keys(answers).length) {
    body.innerHTML = Object.entries(answers)
      .map(([q, a]) => `<div class="perm-answer"><b>${escapeHtml(q)}:</b> ${escapeHtml(Array.isArray(a) ? a.join(", ") : String(a))}</div>`)
      .join("");
  } else {
    body.innerHTML = `<div class="perm-answer">${allow ? "Разрешено" : "Отклонено"}</div>`;
  }
}

// Stash the `result` event's final stats; `finalizeTurn` (called right after,
// see ws.js) folds them into the status line instead of a separate element.
export function addMeta(evt) {
  if (!state.current) return;
  const parts = [];
  if (evt.duration_ms != null) parts.push(`${(evt.duration_ms / 1000).toFixed(1)} с`);
  if (evt.total_cost_usd != null) parts.push(`$${evt.total_cost_usd.toFixed(4)}`);
  const outTokens = evt.usage && evt.usage.output_tokens;
  if (outTokens != null) parts.push(`${fmtTokens(outTokens)} токенов`);
  if (evt.num_turns != null) parts.push(`${evt.num_turns} turns`);
  state.current.finalMetaParts = parts.length ? parts : null;
}

// Turn the live status line into a persistent, collapsed summary once the
// turn ends: no more pulse, final stats instead of the ticking elapsed/tokens
// (falling back to those if the turn never got a `result` — e.g. interrupted
// or errored). The reasoning chevron (if any) stays clickable afterward too.
function finalizeStatusLine(cur) {
  if (!cur.statusEl) return;
  const parts = cur.finalMetaParts || [fmtElapsed(Date.now() - cur.startTime), `${cur.tokens} токенов`];
  const hasDetail = cur.thinkStart != null;
  cur.statusEl.classList.add("done");
  cur.statusEl.classList.toggle("has-detail", hasDetail);
  cur.statusEl.innerHTML =
    `<span class="status-text">${escapeHtml(parts.join(" · "))}</span>` +
    (hasDetail ? `<span class="status-chevron">${CHEVRON_SVG}</span>` : "");
  if (hasDetail) renderStatusDetail(cur); // keep it fresh in case it's already open
}

// `notify: false` skips the chime/desktop-notification (but still does all
// the UI bookkeeping) — used for "exit", which fires on ANY process death
// (explicit interrupt, a crash, or the whole server shutting down for a
// restart) and never actually means "here's your answer".
export function finalizeTurn({ notify = true } = {}) {
  if (state.statusTimer) {
    clearInterval(state.statusTimer);
    state.statusTimer = null;
  }
  // Captured before `state.current` is nulled below, so the notification (sent
  // after) can show a snippet of what the answer actually said.
  const answerPreview = state.current ? state.current.textRaw || "" : "";
  if (state.current) {
    stopThinkingClock(state.current); // freeze the reasoning clock at turn end
    // A turn with text interleaved between tool calls creates a fresh `textEl`
    // per segment (content_block_start resets it), each stamped with "cursor" —
    // clearing only the last one leaves every earlier segment blinking forever.
    if (state.current.answerEl) {
      state.current.answerEl.querySelectorAll(".cursor").forEach((elm) => elm.classList.remove("cursor"));
    }
    state.current.msgEl.classList.remove("streaming");
    finalizeStatusLine(state.current);
    if (state.current.answerEl) addFoldIfLong(state.current.answerEl);
    // Fold this turn's tokens into the chat total so the badge doesn't dip
    // before the authoritative refresh lands.
    if (state.sessionId) {
      const u = (state.chatUsage[state.sessionId] =
        state.chatUsage[state.sessionId] || {
          tokens: 0, input_tokens: 0, cache_read: 0, cache_creation: 0, turns: 0,
        });
      u.tokens += state.current.tokens || 0;
    }
  }
  state.current = null;
  state.streaming = false;
  setStreamingUI(false);
  updateUsageBadge();
  setFaviconState("idle");
  // Reattaching after a reload replays past turns through this same function —
  // only a turn that just finished live should chime/notify.
  if (!state.replayMode && notify) {
    if (settings.sound) playCompletionChime();
    if (settings.notify) notifyTurnComplete((el.title.textContent || "").trim(), answerPreview);
  }
  loadChatList().then(updateUsageBadge); // authoritative token total from the .jsonl
  loadUsage(); // refresh subscription %s at turn end (cached ~20s server-side)
}

// Long answers get a chevron (bottom-right) to collapse to their last ~2 lines.
// Expanded by default.
export const FOLD_THRESHOLD = 220; // px; roughly 9-10 lines
export function addFoldIfLong(answerEl) {
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

// Max message blocks kept in the live view before the oldest are trimmed.
export const LIVE_CAP = 150;

// `target` lets the caller render into a detached fragment (history windowing).
export function makeMessage(role, target = el.messages) {
  const msgEl = document.createElement("div");
  msgEl.className = `msg ${role}`;

  const bodyEl = document.createElement("div");
  bodyEl.className = "body";

  msgEl.appendChild(bodyEl);
  target.appendChild(msgEl);
  if (target === el.messages) capMessages();
  return { msgEl, bodyEl };
}

// Bound the DOM in the live view: keep only the newest LIVE_CAP message blocks.
export function capMessages() {
  const nodes = el.messages.querySelectorAll(":scope > .msg");
  if (nodes.length <= LIVE_CAP) return;
  const remove = nodes.length - LIVE_CAP;
  for (let i = 0; i < remove; i++) nodes[i].remove();
  // Evicting the top of the DOM must advance the windowed-transcript cursor,
  // otherwise "load earlier" desyncs (re-inserts a gap) after history was paged in.
  if (state.transcript) {
    state.transcript.start = Math.min(
      state.transcript.msgs.length,
      state.transcript.start + remove
    );
    updateEarlierButton();
  }
}

// Some entries stored as "user" are actions, not messages (e.g. an interrupt).
// They get the muted service style instead of a user bubble.
export function isServiceText(text) {
  return /^\[Request interrupted by user/.test((text || "").trim());
}

export function addUserMessage(text, images, target = el.messages) {
  clearEmptyState();
  if (isServiceText(text) && !(images && images.length)) {
    showSystem(text, target);
    return;
  }
  const { bodyEl } = makeMessage("user", target);
  if (images && images.length) {
    const row = document.createElement("div");
    row.className = "msg-images";
    for (const im of images) {
      const img = document.createElement("img");
      img.className = "msg-image";
      img.src = `data:${im.media_type};base64,${im.data}`;
      img.alt = "";
      img.addEventListener("click", () => openLightbox(img.src));
      row.appendChild(img);
    }
    bodyEl.appendChild(row);
  }
  if (text) {
    const content = document.createElement("div");
    content.className = "content";
    content.textContent = text;
    bodyEl.appendChild(content);
  }
  // Estimated outgoing token count, captioned below the message.
  const est = estimateTokens(text);
  if (est > 0) {
    const meta = document.createElement("div");
    meta.className = "meta-line out-tokens";
    meta.textContent = `↑ ~${fmtTokens(est)} токенов`;
    bodyEl.appendChild(meta);
  }
  if (target === el.messages) scrollToBottomIfPinned();
}

// Rough token estimate from text length (~4 chars/token). No tokenizer client-side.
export function estimateTokens(text) {
  if (!text) return 0;
  return Math.max(1, Math.round(text.length / 4));
}

export function showSystem(text, target = el.messages) {
  clearEmptyState();
  const { bodyEl } = makeMessage("system", target);
  const content = document.createElement("div");
  content.className = "content";
  content.textContent = text;
  bodyEl.appendChild(content);
  if (target === el.messages) scrollToBottomIfPinned();
}

export function clearEmptyState() {
  const es = el.messages.querySelector(".empty-state");
  if (es) es.remove();
}

// Wipe the transcript view (used when replaying a live session on (re)attach).
export function resetMessages() {
  el.messages.innerHTML = "";
  state.current = null;
}

export function scrollToBottom() {
  el.messages.scrollTop = el.messages.scrollHeight;
  // Set the flag here rather than waiting for the browser's (async) `scroll`
  // event, so the button hides on the same tick instead of one frame later.
  state.followBottom = true;
  scheduleScrollbar();
  updateScrollToBottomButton();
}

// While the user is within this many px of the bottom we keep following new
// output; scroll up past it and auto-follow turns off so they can read earlier
// messages. `state.followBottom` is maintained by the scroll listener below.
const FOLLOW_THRESHOLD = 80;

export function distanceFromBottom() {
  const m = el.messages;
  return m.scrollHeight - m.clientHeight - m.scrollTop;
}

// Keep following only when the caller observed the view at the exact bottom
// before its DOM update. Once the user scrolls up, no streamed update may pull
// them down; clicking the arrow (or returning to the bottom) re-enables it.
export function scrollToBottomIfPinned(wasPinned = state.followBottom) {
  if (wasPinned) {
    scrollToBottom();
  } else {
    scheduleScrollbar();
  }
  updateScrollToBottomButton();
}

// Mirror `state.followBottom` exactly: the button is the visual counterpart of
// auto-follow, so it must appear the instant auto-scroll turns off and vanish
// the instant it turns back on (same FOLLOW_THRESHOLD, no separate zone) —
// otherwise there'd be a dead band where follow is off but the button is still
// hidden, leaving the user with no cue that scrolling stopped following.
export function updateScrollToBottomButton() {
  const btn = el.scrollToBottomBtn;
  if (!btn) return;
  const canScroll = distanceFromBottom() > 0;
  btn.hidden = state.followBottom || !canScroll;
}

// --- Custom scrollbar ---------------------------------------------------------
// Sync the orange thumb's size/position to the message view's scroll state.
export function updateScrollbar() {
  const m = el.messages;
  const ch = m.clientHeight;
  const sh = m.scrollHeight;
  if (sh <= ch + 2) {
    el.scrollbar.hidden = true; // nothing to scroll
    return;
  }
  el.scrollbar.hidden = false;
  const trackH = el.scrollbar.offsetHeight || window.innerHeight * 0.8;
  const thumbH = Math.max(18, Math.round((trackH * ch) / sh));
  const maxScroll = sh - ch;
  const maxTop = trackH - thumbH;
  const top = maxScroll > 0 ? (m.scrollTop / maxScroll) * maxTop : 0;
  el.scrollbarThumb.style.height = thumbH + "px";
  el.scrollbarThumb.style.transform = `translateY(${top}px)`;
}

export let scrollbarRaf = 0;
export function scheduleScrollbar() {
  if (scrollbarRaf) return;
  scrollbarRaf = requestAnimationFrame(() => {
    scrollbarRaf = 0;
    updateScrollbar();
  });
}

el.messages.addEventListener("scroll", () => {
  // The user's scroll position is the single source of truth for follow mode:
  // within FOLLOW_THRESHOLD of the bottom → keep following; scrolled up → stop.
  // (A programmatic scrollToBottom lands at ~0 distance, so it keeps follow on.)
  state.followBottom = distanceFromBottom() <= FOLLOW_THRESHOLD;
  updateScrollbar();
  updateScrollToBottomButton();
}, { passive: true });
window.addEventListener("resize", () => { updateScrollbar(); updateScrollToBottomButton(); });
el.scrollToBottomBtn?.addEventListener("click", () => {
  el.scrollToBottomBtn.blur();
  scrollToBottom();
});
// Content grows during streaming / on chat load without a scroll event.
new MutationObserver(scheduleScrollbar).observe(el.messages, {
  childList: true, subtree: true, characterData: true,
});

// Drag / click the track to scroll (hit area widened via CSS ::before).
export let scrollbarDrag = false;
export function scrollFromPointer(clientY) {
  const rect = el.scrollbar.getBoundingClientRect();
  const thumbH = el.scrollbarThumb.offsetHeight;
  const maxTop = rect.height - thumbH;
  let rel = clientY - rect.top - thumbH / 2;
  rel = Math.max(0, Math.min(maxTop, rel));
  const m = el.messages;
  const maxScroll = m.scrollHeight - m.clientHeight;
  m.scrollTop = maxTop > 0 ? (rel / maxTop) * maxScroll : 0;
}
el.scrollbar.addEventListener("pointerdown", (e) => {
  scrollbarDrag = true;
  el.scrollbar.setPointerCapture(e.pointerId);
  scrollFromPointer(e.clientY);
  updateScrollToBottomButton();
  e.preventDefault();
});
el.scrollbar.addEventListener("pointermove", (e) => {
  if (scrollbarDrag) {
    scrollFromPointer(e.clientY);
    updateScrollToBottomButton();
  }
});
export const endScrollbarDrag = (e) => {
  scrollbarDrag = false;
  try { el.scrollbar.releasePointerCapture(e.pointerId); } catch (_) {}
  updateScrollToBottomButton();
};
el.scrollbar.addEventListener("pointerup", endScrollbarDrag);
el.scrollbar.addEventListener("pointercancel", endScrollbarDrag);

export function setStreamingUI(on) {
  el.input.disabled = false;
  updateSendButton();
  // Export/delete would otherwise act on a chat mid-turn — the exported file
  // would be missing the reply still being streamed. Hide the controls for
  // the duration; refreshComposerState() governs their visibility otherwise.
  el.chatControls.hidden = on || !state.sessionId;
}

// Compact token count: 10, 100, 1k, 5k, 100k, 1m, 1.5m …
export function fmtTokens(n) {
  n = Math.round(n || 0);
  if (n < 1000) return String(n);
  const trim = (x) => (Math.round(x * 10) / 10).toString().replace(/\.0$/, "");
  if (n < 999_500) return trim(n / 1000) + "k"; // 999_500+ rounds up to "1M"
  return trim(n / 1_000_000) + "M";
}

// The badge shows the open chat's running token total (base from the backend +
// the live tokens of a turn in progress). Hidden when no chat is open.
export function updateUsageBadge() {
  const open = !!state.sessionId;
  el.usageBadge.hidden = !open;
  if (!open) return;
  const u = state.chatUsage[state.sessionId];
  const base = (u && u.tokens) || 0;
  const live = state.current ? state.current.tokens || 0 : 0;
  const tokens = fmtTokens(base + live);
  const g = state.usage; // subscription limits (session/week/fable %), refreshed rarely
  if (g) {
    const pct = (o) => (o && o.percent != null ? o.percent : 0) + "%";
    el.usageBadge.classList.add("multi");
    el.usageBadge.innerHTML =
      `<span class="ub-pct" title="Сессия (5 ч)">${pct(g.session)}</span>` +
      `<span class="ub-pct" title="Неделя (все модели)">${pct(g.week)}</span>` +
      `<span class="ub-pct ub-dim" title="Неделя (Fable)">${pct(g.fable)}</span>` +
      `<span class="ub-tok" title="Токены в этом чате">${tokens}</span>`;
  } else {
    el.usageBadge.classList.remove("multi");
    el.usageBadge.textContent = tokens;
  }
  if (el.usagePanel.classList.contains("open")) renderUsageDetail();
}

// ---------------------------------------------------------------------------
// Subscription usage/limits (5-hour "session" window + weekly + Fable weekly),
// from /api/usage (which shells out to the CLI's own `/usage`; free — no turn).
// Refreshed RARELY: on turn end, when the usage/settings panels open, and once
// at startup. Renders into the badge, the settings section, and the sidebar.
// ---------------------------------------------------------------------------
export async function loadUsage() {
  let data = null;
  try { data = await (await fetch("/api/usage")).json(); } catch (e) {}
  state.usage = data && data.available ? data : null;
  updateUsageBadge();
  if (el.usagePanel.classList.contains("open")) renderUsageDetail();
}

// One "Label … NN%" row with a thin progress bar. `compact` drops the reset line.
function limRow(label, o, compact) {
  if (!o) return "";
  const pct = Math.max(0, Math.min(100, Math.round(o.percent || 0)));
  const cls = pct >= 90 ? " hot" : pct >= 70 ? " warm" : "";
  const reset = !compact && o.resets
    ? `<div class="usage-reset">сброс: ${escapeHtml(o.resets)}</div>` : "";
  return `<div class="usage-lim">
    <div class="usage-lim-top"><span>${label}</span><span>${pct}%</span></div>
    <div class="usage-track"><div class="usage-fill${cls}" style="width:${pct}%"></div></div>
    ${reset}
  </div>`;
}

function subscriptionHeading(g) {
  const plan = g.plan ? " " + escapeHtml(String(g.plan).toUpperCase()) : "";
  return `Подписка${plan}`;
}

// Full n with thousands separators, e.g. 1 234 567.
export function fmtFull(n) {
  return String(Math.round(n || 0)).replace(/\B(?=(\d{3})+(?!\d))/g, " ");
}

export function renderUsageDetail() {
  const u = state.chatUsage[state.sessionId] || {};
  const live = state.current ? state.current.tokens || 0 : 0;
  const output = (u.tokens || 0) + live;
  const input = u.input_tokens || 0;
  const total = output + input;

  // Subscription limits section (moved here from settings) — shown when known.
  const g = state.usage;
  const limits = g
    ? `<div class="usage-section usage-limits">
         <div class="usage-section-head">${iIcon("target", 15, "usage-ic")}<span>${subscriptionHeading(g)}</span></div>
         ${limRow("Сессия (5 ч)", g.session, false)}
         ${limRow("Неделя", g.week, false)}
         ${limRow("Неделя (Fable)", g.fable, false)}
       </div>`
    : "";

  const row = (icon, k, v, sub, cls) =>
    `<div class="usage-row ${cls || ""}">
       <span class="usage-ic">${iIcon(icon, 16)}</span>
       <div class="usage-row-main"><div class="k">${k}</div>${
         sub ? `<div class="usage-sub">${sub}</div>` : ""
       }</div>
       <div class="v">${v}</div>
     </div>`;

  el.usageDetail.innerHTML =
    limits +
    `<div class="usage-section">
       <div class="usage-section-head">${iIcon("chart", 15, "usage-ic")}<span>Токены этого чата</span></div>` +
    row("calculator", "Всего токенов", fmtFull(total), "вход + выход", "total") +
    row("robot", "Ответы модели (output)", fmtFull(output), "размышления + ответ вместе") +
    row("person", "Входные (input)", fmtFull(input)) +
    row("bolt", "Из кэша", fmtFull(u.cache_read || 0), "дешевле обычного входа") +
    row("archive", "Создание кэша", fmtFull(u.cache_creation || 0)) +
    row("bubble", "Ходов модели", fmtFull(u.turns || 0)) +
    `</div>`;
}

export function setUsage(open) {
  if (open && !state.sessionId) return; // nothing to show without an open chat
  el.usagePanel.classList.toggle("open", open);
  el.usageOverlay.hidden = !open;
  if (open) { renderUsageDetail(); loadUsage(); hideBadge(el.usageBadge); hideBadge(el.settingsBadge); }
  else { showBadgeSoon(el.usageBadge); showBadgeSoon(el.settingsBadge); }
}

// Keep send in its fixed place; expose the stop button beside it while the
// assistant is streaming. This prevents the composer controls from jumping.
export function updateSendButton() {
  const empty =
    el.input.value.trim().length === 0 &&
    state.pendingImages.length === 0 &&
    state.pendingFiles.length === 0;
  el.send.hidden = false;
  el.send.disabled = state.streaming || empty || chatFrozen(state.sessionId);
  el.stop.hidden = !state.streaming;
}

// Paste an image from the clipboard (Ctrl+V) → attach it to the next message.
el.input.addEventListener("paste", (e) => {
  const items = e.clipboardData && e.clipboardData.items;
  if (!items) return;
  const imageItems = [...items].filter(
    (it) => it.kind === "file" && it.type.startsWith("image/")
  );
  if (!imageItems.length) return;
  e.preventDefault(); // do not paste the image as a file path / text
  for (const it of imageItems) {
    const file = it.getAsFile();
    if (file) addImageFile(file);
  }
});

// Read an image File → data URL → attach to the next message.
export function addImageFile(file) {
  const reader = new FileReader();
  reader.onload = () => {
    const url = String(reader.result); // data:<mime>;base64,<data>
    const data = url.slice(url.indexOf(",") + 1);
    state.pendingImages.push({ media_type: file.type || "image/png", data, url });
    renderAttachPreview();
    updateSendButton();
  };
  reader.readAsDataURL(file);
}

// --- File attachments (📎 button + drag-and-drop) --------------------------
// Text files are read and inlined into the prompt on send (works for both the
// CLI and native engines — it's just text). Images route to pendingImages.
// Binary/non-text files are rejected (we can't inline them meaningfully).
const ATTACH_CHAR_CAP = 200000; // ~200k chars inlined per file
const TEXT_EXT =
  /\.(txt|md|markdown|json|jsonl|ya?ml|toml|ini|conf|cfg|csv|tsv|log|rs|js|jsx|mjs|cjs|ts|tsx|py|rb|go|java|kt|kts|c|h|cpp|hpp|cc|cs|php|sh|bash|zsh|fish|sql|html?|css|scss|sass|less|xml|svg|vue|svelte|dart|swift|scala|lua|r|pl|pm|ex|exs|erl|clj|hs|ml|elm|nim|zig|gradle|properties|env|gitignore|dockerfile|makefile)$/i;

function looksTextual(file) {
  return (
    (file.type && file.type.startsWith("text/")) ||
    file.type === "application/json" ||
    file.type === "application/xml" ||
    file.type === "image/svg+xml" ||
    TEXT_EXT.test(file.name) ||
    file.type === "" // unknown — the NUL check after reading is the backstop
  );
}

export function addFiles(files) {
  for (const file of files) {
    if (file.type.startsWith("image/") && file.type !== "image/svg+xml") {
      addImageFile(file);
      continue;
    }
    if (!looksTextual(file)) {
      showSystem(`Файл «${file.name}» не текстовый — вложение пропущено.`);
      continue;
    }
    const reader = new FileReader();
    reader.onload = () => {
      let text = String(reader.result);
      if (text.slice(0, 8192).includes("\u0000")) {
        showSystem(`Файл «${file.name}» выглядит бинарным — вложение пропущено.`);
        return;
      }
      let truncated = false;
      if (text.length > ATTACH_CHAR_CAP) {
        text = text.slice(0, ATTACH_CHAR_CAP);
        truncated = true;
      }
      state.pendingFiles.push({ name: file.name, text, truncated });
      renderAttachPreview();
      updateSendButton();
    };
    reader.readAsText(file);
  }
}

el.attachBtn.addEventListener("click", () => el.fileInput.click());
el.fileInput.addEventListener("change", () => {
  if (el.fileInput.files && el.fileInput.files.length) addFiles(el.fileInput.files);
  el.fileInput.value = ""; // allow re-selecting the same file
});

// Drag-and-drop anywhere over the app attaches the dropped files.
["dragenter", "dragover"].forEach((ev) =>
  document.addEventListener(ev, (e) => {
    if (e.dataTransfer && [...e.dataTransfer.types].includes("Files")) {
      e.preventDefault();
      document.body.classList.add("drag-over");
    }
  })
);
["dragleave", "drop"].forEach((ev) =>
  document.addEventListener(ev, (e) => {
    // Only clear when the pointer actually leaves the window (relatedTarget null).
    if (ev === "drop" || !e.relatedTarget) document.body.classList.remove("drag-over");
  })
);
document.addEventListener("drop", (e) => {
  if (e.dataTransfer && e.dataTransfer.files && e.dataTransfer.files.length) {
    e.preventDefault();
    addFiles(e.dataTransfer.files);
  }
});

export function renderAttachPreview() {
  const box = el.attachPreview;
  box.innerHTML = "";
  state.pendingImages.forEach((img, i) => {
    const thumb = document.createElement("div");
    thumb.className = "attach-thumb";
    const im = document.createElement("img");
    im.src = img.url;
    im.alt = "";
    im.addEventListener("click", () => openLightbox(img.url));
    const rm = document.createElement("button");
    rm.className = "rm";
    rm.type = "button";
    rm.innerHTML = iIcon('close', 12);
    rm.title = "Убрать";
    rm.addEventListener("click", () => {
      state.pendingImages.splice(i, 1);
      renderAttachPreview();
      updateSendButton();
    });
    thumb.appendChild(im);
    thumb.appendChild(rm);
    box.appendChild(thumb);
  });
  state.pendingFiles.forEach((f, i) => {
    const chip = document.createElement("div");
    chip.className = "attach-file";
    const name = document.createElement("span");
    name.className = "attach-file-name";
    name.innerHTML = `${iIcon('file', 14, 'inline')} ${escapeHtml(f.name)}` + (f.truncated ? " (обрезан)" : "");
    name.title = f.name;
    const rm = document.createElement("button");
    rm.className = "rm";
    rm.type = "button";
    rm.innerHTML = iIcon('close', 12);
    rm.title = "Убрать";
    rm.addEventListener("click", () => {
      state.pendingFiles.splice(i, 1);
      renderAttachPreview();
      updateSendButton();
    });
    chip.appendChild(name);
    chip.appendChild(rm);
    box.appendChild(chip);
  });
  box.hidden = state.pendingImages.length === 0 && state.pendingFiles.length === 0;
}

// Zoom an image over everything; click anywhere to return it to its place.
export function openLightbox(src) {
  let lb = document.getElementById("lightbox");
  if (!lb) {
    lb = document.createElement("div");
    lb.id = "lightbox";
    lb.className = "lightbox";
    lb.innerHTML = '<img alt="" />';
    lb.addEventListener("click", () => (lb.hidden = true));
    document.body.appendChild(lb);
  }
  lb.querySelector("img").src = src;
  lb.hidden = false;
}
