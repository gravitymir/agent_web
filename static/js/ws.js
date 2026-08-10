import { state, el } from './state.js';
import { ensureAssistant, updateStatus, appendText, ensureThinking, appendThinkingText, stopThinkingClock, setThinkingTokens, renderToolCalls, addMeta, finalizeTurn, addUserMessage, showSystem, resetMessages, renderMsgRange, scrollToBottom, updateUsageBadge, isServiceText, renderPermissionRequest, markPermissionResolved, loadUsage } from './render.js';
import { setFaviconState } from '../favicon.js';
import { renderTranscriptWindowed, restoreUnsentMessage, confirmSentMessage, loadProviders, loadChatList } from './ui.js';

// ---------------------------------------------------------------------------
// WebSocket connection with exponential-backoff auto-reconnect.
// ---------------------------------------------------------------------------
const INITIAL_DELAY = 1500;   // ms
const MAX_DELAY = 30000;      // ms
const HEARTBEAT_INTERVAL = 20000; // ms

let reconnectAttempts = 0;
let reconnectTimer = null;
// Whether we've had at least one successful connection. Distinguishes the very
// first connect (startup already fetched providers/models/usage) from a later
// RE-connect, where the server may have restarted on a different engine.
let connectedOnce = false;
let heartbeatTimer = null;
let intentionalClose = false;
// Whether the keeper we just attached to had ANY live scrollback at all (the
// server's own `replay` flag — a plain non-empty/empty check, not a count).
let hadLiveReplay = false;
// Whether a "send" that looked successful (readyState was OPEN) is still
// awaiting its own "cwi:user" echo. `readyState` can still read OPEN for a
// moment after the server process actually died — the browser doesn't always
// notice a dead socket right away — so `send()` can silently vanish into a
// connection that's already gone. The composer holds its text/attachments
// (see ui.js's `lockComposerForConfirmation`) until this resolves either way:
// confirmed (`confirmSentMessage`) or the connection dropped first
// (`restoreUnsentMessage` — nothing to restore, it was never cleared).
let awaitingConfirmation = false;

// Call right after `sendWs(payload)` reports success for a chat message.
export function markSentPending() {
  awaitingConfirmation = true;
}

export function connect() {
  clearTimeout(reconnectTimer);
  reconnectTimer = null;

  const proto = location.protocol === "https:" ? "wss" : "ws";
  const ws = new WebSocket(`${proto}://${location.host}/ws`);
  state.ws = ws;

  ws.onopen = () => {
    const reconnected = connectedOnce;
    connectedOnce = true;
    reconnectAttempts = 0;
    setConn(true);
    startHeartbeat(ws);
    // Reconnect: re-attach to the live session (if any) to resume its stream.
    if (state.sessionId && !state.isNew) {
      sendWs({ type: "attach", session_id: state.sessionId });
    }
    // On a RE-connect the server may have been restarted on a different engine
    // (CWI_ENGINE). Re-resolve provider/engine, models, usage and the chat list
    // so the UI stops showing the previous engine without a manual page reload.
    if (reconnected) {
      loadProviders();
      loadUsage();
      loadChatList();
    }
  };

  ws.onclose = () => {
    stopHeartbeat();
    setConn(false);
    if (awaitingConfirmation) {
      restoreUnsentMessage();
      awaitingConfirmation = false;
    }
    if (!intentionalClose) {
      scheduleReconnect();
    }
    intentionalClose = false;
  };

  ws.onerror = (e) => {
    console.warn("WebSocket error", e);
    // Closing triggers onclose, which schedules the reconnect.
    ws.close();
  };

  ws.onmessage = (e) => {
    let evt;
    try { evt = JSON.parse(e.data); } catch { return; }
    handleEvent(evt);
  };
}

export function disconnect() {
  intentionalClose = true;
  clearTimeout(reconnectTimer);
  reconnectTimer = null;
  stopHeartbeat();
  if (state.ws) {
    state.ws.close();
    state.ws = null;
  }
  setConn(false);
}

function scheduleReconnect() {
  if (reconnectTimer) return;
  const delay = Math.min(INITIAL_DELAY * Math.pow(2, reconnectAttempts), MAX_DELAY);
  reconnectAttempts += 1;
  updateOfflineBanner(delay, reconnectAttempts);
  reconnectTimer = setTimeout(() => {
    reconnectTimer = null;
    connect();
  }, delay);
}

export function setConn(online) {
  el.offlineBanner.hidden = online;
  if (online) {
    el.offlineDetail.textContent = "";
  }
}

el.offlineBanner.addEventListener("click", () => {
  if (el.offlineBanner.hidden) return;
  // Cancel the scheduled backoff and retry immediately.
  clearTimeout(reconnectTimer);
  reconnectTimer = null;
  updateOfflineBanner(0, reconnectAttempts + 1);
  connect();
});

function updateOfflineBanner(nextDelayMs, attempt) {
  if (nextDelayMs <= 0) {
    el.offlineDetail.textContent = `попытка ${attempt} · сейчас…`;
  } else {
    const secs = Math.round(nextDelayMs / 1000);
    el.offlineDetail.textContent = `через ${secs} с · попытка ${attempt}`;
  }
}

function startHeartbeat(ws) {
  stopHeartbeat();
  heartbeatTimer = setInterval(() => {
    if (ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({ type: "ping" }));
    }
  }, HEARTBEAT_INTERVAL);
}

function stopHeartbeat() {
  if (heartbeatTimer) {
    clearInterval(heartbeatTimer);
    heartbeatTimer = null;
  }
}

export function sendWs(obj) {
  if (state.ws && state.ws.readyState === WebSocket.OPEN) {
    state.ws.send(JSON.stringify(obj));
    return true;
  }
  return false;
}

// ---------------------------------------------------------------------------
// Event handling (control frames + Claude stream-json)
// ---------------------------------------------------------------------------
export function handleEvent(evt) {
  if (evt.cwi) {
    switch (evt.cwi) {
      case "session":
        state.sessionId = evt.session_id;
        // A replay means we're (re)attaching to a live session: rebuild the
        // view from the scrollback that follows.
        hadLiveReplay = !!evt.replay;
        if (evt.replay) {
          state.replayMode = true;
          resetMessages();
        }
        break;
      case "replay_end":
        state.replayMode = false;
        // A brand-new keeper (nothing live to show — e.g. just re-spawned via
        // --resume) has an empty scrollback; restore the on-disk history so
        // the user doesn't land on a blank chat. But if there WAS live replay
        // (the same keeper survived a reload, mid-turn or not), trust it fully
        // — it's strictly more current than disk (a pending permission_request,
        // for instance, only ever exists live, never in the on-disk transcript).
        // Comparing message COUNTS here used to decide this instead, but raw
        // live events and on-disk logical messages are different units (one
        // "assistant" turn can emit many raw stream events) and could misfire
        // in either direction — replacing live state with stale disk state.
        if (state.transcript && !hadLiveReplay) {
          // Re-render windowed (last MAX_RENDERED + "load earlier"), not the whole
          // transcript — otherwise a reconnect on a long chat unbounds the DOM.
          renderTranscriptWindowed(state.transcript.msgs);
        }
        break;
      case "user": {
        // A user turn, echoed by the keeper (also seen by other viewers). Any
        // echo proves this connection's own send actually reached the server —
        // safe now to clear the held composer for real (see `markSentPending`).
        if (awaitingConfirmation) {
          awaitingConfirmation = false;
          confirmSentMessage();
        }
        // Only finalize a *previous* assistant turn; don't reset the composer
        // for our own just-sent message (no current turn yet).
        if (state.current) finalizeTurn();
        const text = evt.text || "";
        addUserMessage(text, evt.images || []);
        // Show the "печатает…" placeholder the moment the send is confirmed
        // (this echo), rather than waiting for the model's first token — closes
        // the silent gap during process spin-up / first-token latency. Skipped
        // for a synthetic interrupt marker: that ends a turn, it doesn't start one.
        if (!state.replayMode && !isServiceText(text)) ensureAssistant();
        break;
      }
      case "think":
        // Authoritative reasoning duration (survives replay) → freeze the timer.
        if (state.current && state.current.thinkStart != null) {
          state.current.thinkMs = evt.ms;
          stopThinkingClock(state.current);
          updateStatus();
        }
        break;
      case "exit":
        // Never a real "answer ready": fires on an explicit interrupt (the
        // user is already at the keyboard), a crash, or the server itself
        // shutting down for a restart — none of those earned a chime.
        finalizeTurn({ notify: false });
        break;
      case "executor":
        // Executor VM control status/progress — re-emit for the guest-server tab
        // (guest.js listens; a DOM event avoids a ws.js↔guest.js import cycle).
        window.dispatchEvent(new CustomEvent("cwi-executor", { detail: evt }));
        break;
      case "no_session":
        break; // nothing live to attach to; keep whatever is shown
      case "error":
        // Surface the error in-chat and don't count the turn (no stats footer).
        showSystem(turnErrorMessage(evt.message), undefined, true);
        finalizeTurn({ notify: false, error: true });
        break;
      case "permission_request":
        // Rendered regardless of replay: an unanswered request is still
        // actionable state after a reload, not a one-off notification.
        renderPermissionRequest(evt);
        break;
      case "permission_resolved":
        markPermissionResolved(evt.request_id, evt.allow, evt.answers, evt.response);
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

    case "assistant":
      // Render each tool call with its parameters, and count them as running.
      if (state.current) {
        renderToolCalls(state.current, evt);
        const n = countBlocks(evt, "tool_use");
        if (n) { state.current.runningTasks += n; updateStatus(); setFaviconState(toolFaviconState(firstToolName(evt))); }
      }
      // Context-window fill: each assistant message's own usage reflects the
      // conversation size as of THAT api call (not summed — a later, smaller
      // number would be wrong going backwards, but it never does within a
      // chat: context only grows). Updates live, once per step, not just at
      // turn end, since a single turn can span several tool-loop steps.
      if (state.sessionId && evt.message && evt.message.usage) {
        const us = evt.message.usage;
        const ctx = (us.input_tokens || 0) + (us.cache_read_input_tokens || 0) + (us.cache_creation_input_tokens || 0);
        const u = (state.chatUsage[state.sessionId] = state.chatUsage[state.sessionId] || {});
        u.contextTokens = ctx;
        updateUsageBadge();
      }
      break;

    case "user":
      // Tool results coming back — those tasks finished.
      if (state.current) {
        const n = countBlocks(evt, "tool_result");
        if (n) {
          state.current.runningTasks = Math.max(0, state.current.runningTasks - n);
          updateStatus();
          if (state.current.runningTasks === 0) setFaviconState("thinking");
        }
      }
      break;

    case "result":
      if (resultIsError(evt)) {
        // Failed turn: show the error, drop the stats footer, don't count it.
        showSystem(turnErrorMessage(evt.result || evt.error || evt.message), undefined, true);
        finalizeTurn({ notify: false, error: true });
      } else {
        if (evt.total_cost_usd != null || evt.duration_ms != null) {
          addMeta(evt);
        }
        finalizeTurn();
      }
      break;
  }
}

export function countBlocks(evt, type) {
  const content = evt.message && evt.message.content;
  return Array.isArray(content) ? content.filter((b) => b.type === type).length : 0;
}

// Map a tool name to a favicon activity state (colour). Unknown tools fall back
// to the generic "tool" colour.
export function toolFaviconState(name) {
  switch (name) {
    case "Read": case "NotebookRead": case "WebFetch":
      return "read";
    case "Grep": case "Glob": case "WebSearch": case "LS":
      return "search";
    case "Write": case "Edit": case "MultiEdit": case "NotebookEdit":
      return "write";
    default:
      return "tool";
  }
}

function firstToolName(evt) {
  const content = evt.message && evt.message.content;
  if (!Array.isArray(content)) return null;
  const b = content.find((x) => x.type === "tool_use");
  return b ? b.name : null;
}

// Turn a raw backend/agent error into a clear, user-facing message. Auth / missing
// key failures get a dedicated explanation since that's the common case.
function turnErrorMessage(raw) {
  const s = (raw == null ? "" : String(raw)).trim();
  if (/oauth|authenticat|unauthorized|\b401\b|api[_ -]?key|token has expired|re-?authenticate|no api key/i.test(s)) {
    return "Ошибка авторизации: нет действующего ключа API или токен истёк. " +
      "Проверьте ключ в настройках (или переменную окружения CWI_AGENT_*_API_KEY) и переавторизуйтесь.";
  }
  return s ? `Ошибка: ${s}` : "Ошибка: запрос завершился неуспешно.";
}

// A `result` event that represents a failed turn (Claude Code sets is_error /
// an "error…" subtype; our native agent may too).
function resultIsError(evt) {
  return evt.is_error === true ||
    (typeof evt.subtype === "string" && evt.subtype.indexOf("error") !== -1);
}

export function handleStreamEvent(ev) {
  if (!ev || !ev.type) return;
  const cur = ensureAssistant();

  switch (ev.type) {
    case "content_block_start": {
      // A new block begins: close the active text run.
      cur.textEl = null;
      const block = ev.content_block || {};
      if (block.type === "tool_use") {
        // The full parameters arrive with the `assistant` event; here we just
        // reflect that a tool is running in the live status line.
        cur.status = `выполняет ${block.name || "tool"}…`;
        setFaviconState(toolFaviconState(block.name));
      } else if (block.type === "thinking") {
        ensureThinking(cur);
        cur.status = "размышляет…";
        setFaviconState("thinking");
      }
      updateStatus();
      break;
    }
    case "content_block_delta": {
      const d = ev.delta || {};
      if (d.type === "text_delta" && d.text) {
        appendText(cur, d.text);
        cur.status = "печатает…";
        setFaviconState("output");
      } else if (d.type === "thinking_delta") {
        ensureThinking(cur);
        // CLI mode gives a token estimate; the native engine gives reasoning
        // text — estimate its tokens (~4 chars/token) and count them live.
        if (d.estimated_tokens != null) setThinkingTokens(cur, d.estimated_tokens);
        if (d.thinking) {
          cur.thinkChars = (cur.thinkChars || 0) + d.thinking.length;
          setThinkingTokens(cur, Math.round(cur.thinkChars / 4));
          appendThinkingText(cur, d.thinking);
        }
        cur.status = "размышляет…";
        setFaviconState("thinking");
      }
      break;
    }
    case "message_delta": {
      // Cumulative output-token count for the turn.
      if (ev.usage && ev.usage.output_tokens != null) {
        cur.tokens = ev.usage.output_tokens;
        updateStatus();
        updateUsageBadge(); // live-tick the per-chat counter
      }
      break;
    }
  }
}
