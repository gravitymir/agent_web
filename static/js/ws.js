import { state, el } from './state.js';
import { ensureAssistant, updateStatus, appendText, ensureThinking, appendThinkingText, stopThinkingClock, setThinkingTokens, renderToolCalls, addMeta, finalizeTurn, addUserMessage, showSystem, resetMessages, renderMsgRange, scrollToBottom, updateUsageBadge, isServiceText } from './render.js';
import { setFaviconState } from '../favicon.js';
import { renderTranscriptWindowed } from './ui.js';

// ---------------------------------------------------------------------------
// WebSocket connection with exponential-backoff auto-reconnect.
// ---------------------------------------------------------------------------
const INITIAL_DELAY = 1500;   // ms
const MAX_DELAY = 30000;      // ms
const HEARTBEAT_INTERVAL = 20000; // ms

let reconnectAttempts = 0;
let reconnectTimer = null;
let heartbeatTimer = null;
let intentionalClose = false;
let replayMsgCount = 0;

export function connect() {
  clearTimeout(reconnectTimer);
  reconnectTimer = null;

  const proto = location.protocol === "https:" ? "wss" : "ws";
  const ws = new WebSocket(`${proto}://${location.host}/ws`);
  state.ws = ws;

  ws.onopen = () => {
    reconnectAttempts = 0;
    setConn(true);
    startHeartbeat(ws);
    // Reconnect: re-attach to the live session (if any) to resume its stream.
    if (state.sessionId && !state.isNew) {
      sendWs({ type: "attach", session_id: state.sessionId });
    }
    // If any messages were queued while offline, send them now.
    flushPendingSends();
  };

  ws.onclose = () => {
    stopHeartbeat();
    setConn(false);
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

// Queue a message to be sent once the socket comes back. The existing reconnect
// loop (or the currently opening socket) will flush the queue on the next open event.
export function sendWsOrQueue(obj) {
  if (sendWs(obj)) return true;
  state.pendingSends.push(obj);
  // If the socket is completely down and no reconnect is in flight, kick one off now.
  if (!state.ws && !reconnectTimer) {
    reconnectAttempts = 0;
    connect();
  }
  return false;
}

function flushPendingSends() {
  while (state.pendingSends.length) {
    const obj = state.pendingSends[0];
    if (!sendWs(obj)) break;
    state.pendingSends.shift();
  }
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
        if (evt.replay) {
          state.replayMode = true;
          replayMsgCount = 0;
          resetMessages();
        }
        break;
      case "replay_end":
        state.replayMode = false;
        // The live scrollback only covers the keeper's lifetime. If it's
        // shorter than the on-disk history, restore the full disk history so
        // the user doesn't lose earlier turns after a page reload.
        if (state.transcript && replayMsgCount < state.transcript.msgs.length) {
          // Re-render windowed (last MAX_RENDERED + "load earlier"), not the whole
          // transcript — otherwise a reconnect on a long chat unbounds the DOM.
          renderTranscriptWindowed(state.transcript.msgs);
        }
        replayMsgCount = 0;
        break;
      case "user": {
        // A user turn, echoed by the keeper (also seen by other viewers).
        // Only finalize a *previous* assistant turn; don't reset the composer
        // for our own just-sent message (no current turn yet).
        if (state.replayMode) replayMsgCount++;
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

    case "assistant":
      // Render each tool call with its parameters, and count them as running.
      if (state.replayMode) replayMsgCount++;
      if (state.current) {
        renderToolCalls(state.current, evt);
        const n = countBlocks(evt, "tool_use");
        if (n) { state.current.runningTasks += n; updateStatus(); setFaviconState("tool"); }
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
      if (evt.total_cost_usd != null || evt.duration_ms != null) {
        addMeta(evt);
      }
      finalizeTurn();
      break;
  }
}

export function countBlocks(evt, type) {
  const content = evt.message && evt.message.content;
  return Array.isArray(content) ? content.filter((b) => b.type === type).length : 0;
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
        setFaviconState("tool");
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
