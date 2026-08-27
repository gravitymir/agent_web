// Party (room) drawer: the human side-chat, presence roster, and control hand-off
// (driver ↔ observer). All traffic rides the session's existing WebSocket:
//   → set_identity / party_chat / take_control / release_control (client sends)
//   ← cwi-role / cwi-roster / cwi-party-chat (server frames, re-emitted by ws.js)
// The panel open/close lives in ui.js (setPartyPanel); this owns the contents.
// Everything is rendered via textContent — never innerHTML — since names and
// messages come from other (untrusted) participants.
import { el, state } from "./state.js";
import { setPartyPanel } from "./ui.js";
import { sendWs } from "./ws.js";
import { updateSendButton } from "./render.js";
import { playChatBlip } from "../sound.js";

const NAME_KEY = "cwi_party_name";

// Mirror the server's name allowlist (sanitize_name in session.rs): letters
// (Latin + Cyrillic), digits, space, and a few safe marks — everything else
// (emoji, zalgo/combining marks, bidi overrides, control) is dropped. Capped at
// 10 here (the product rule); the server re-sanitizes at its own hard ceiling.
function sanitizeName(raw) {
  let out = "";
  let prevSpace = false;
  let n = 0;
  for (const ch of [...String(raw)]) {
    const code = ch.codePointAt(0);
    const ok =
      /[A-Za-z0-9]/.test(ch) ||
      (code >= 0x0400 && code <= 0x04ff && /\p{L}/u.test(ch)) ||
      " -_.#!?".includes(ch);
    if (!ok) continue;
    if (ch === " ") {
      if (prevSpace || n === 0) continue;
      prevSpace = true;
    } else {
      prevSpace = false;
    }
    out += ch;
    n += 1;
    if (n >= 10) break;
  }
  return out;
}

function storedName() {
  try {
    return localStorage.getItem(NAME_KEY) || "";
  } catch {
    return "";
  }
}
function storeName(name) {
  try {
    localStorage.setItem(NAME_KEY, name);
  } catch {
    /* private mode — fine, we just re-ask next time */
  }
}

// Tell the server this connection's chosen name (empty → it auto-names "Гость N").
// Remember it locally too, so our own messages are recognised across reloads.
function sendIdentity() {
  state.partyName = storedName();
  sendWs({ type: "set_identity", name: storedName() });
}

// --- Role: gate the AGENT composer off the current role (observers can watch
// the agent but not drive it on a gated instance). The room control buttons are
// gone; who drives is just shown in the chat header (see updateDriverHead).
function applyRole() {
  const driver = state.partyRole === "driver";
  el.input.placeholder =
    state.gated && !driver ? "Сейчас за рулём другой участник…" : "Спросите что-нибудь…";
  updateSendButton();
}

// Live headcount in the chat header: everyone currently in the room (you +
// observers), straight from the roster length. Hidden until there's a room.
function updateOnlineCount(members) {
  const n = (members || []).length;
  // Header pill (inside the open drawer).
  const c = el.partyOnline;
  if (c) {
    if (n > 0) {
      c.innerHTML = `<span class="dot"></span>${n}`;
      c.title = `Людей онлайн: ${n}`;
      c.hidden = false;
    } else {
      c.hidden = true;
    }
  }
  // Collapsed badge (visible even while the drawer is closed): grow it into a
  // pill with the count beside the icon, so viewers are visible at a glance.
  if (el.partyBadge && el.partyBadgeCount) {
    if (n > 0) {
      el.partyBadgeCount.textContent = String(n);
      el.partyBadgeCount.hidden = false;
      el.partyBadge.classList.add("has-count");
      el.partyBadge.title = `Чат · ${n} онлайн`;
    } else {
      el.partyBadgeCount.hidden = true;
      el.partyBadge.classList.remove("has-count");
      el.partyBadge.title = "Чат";
    }
  }
}

// Show who currently drives the agent in the chat header (flag + name), from the
// roster. Hidden when there's no driver / no session.
function updateDriverHead(members) {
  const d = (members || []).find((m) => m.driver);
  if (d) {
    state.partyDriver = d.name;
    nameLine(el.partyDriver, { flag: d.flag, name: d.name });
    el.partyDriver.hidden = false;
  } else {
    state.partyDriver = "";
    el.partyDriver.hidden = true;
  }
}

// --- Human side-chat ----------------------------------------------------------
// Scroll-follow, mirroring the main message view: new messages auto-scroll to
// the bottom ONLY while the reader is at (near) the bottom. Once they scroll up
// to read older messages, nothing yanks them down; returning to the bottom
// re-enables following. The native scrollbars are hidden (CSS), like `.messages`.
const PARTY_FOLLOW_THRESHOLD = 80;
let partyFollow = true;

function partyDistanceFromBottom() {
  const l = el.partyLog;
  return l.scrollHeight - l.clientHeight - l.scrollTop;
}
function partyScrollToBottom() {
  el.partyLog.scrollTop = el.partyLog.scrollHeight;
  partyFollow = true;
}
// The reader's own scroll position is the single source of truth for follow.
el.partyLog.addEventListener(
  "scroll",
  () => { partyFollow = partyDistanceFromBottom() <= PARTY_FOLLOW_THRESHOLD; },
  { passive: true },
);

function autoGrow() {
  el.partyInput.style.height = "auto";
  el.partyInput.style.height = `${Math.min(el.partyInput.scrollHeight, 120)}px`;
}

// Decode a flag emoji (two regional-indicator chars) back to its ISO code, so we
// can render a readable badge on platforms (Windows) that don't draw flag emoji.
function flagCode(flag) {
  if (!flag) return "";
  const cps = [...flag].map((c) => c.codePointAt(0));
  if (cps.length === 2 && cps.every((c) => c >= 0x1f1e6 && c <= 0x1f1ff)) {
    return cps.map((c) => String.fromCharCode(65 + (c - 0x1f1e6))).join("");
  }
  return "";
}

// Build a name line: optional 🚗 (driver), a country flag, then the name — all
// via nodes (never innerHTML; names are untrusted). The flag is a vendored SVG
// (/vendor/flags/<cc>.svg, flag-icons) so it renders identically everywhere —
// Windows draws no flag emoji at all, and phones draw them differently — with
// the emoji kept only as the fallback for non-country flags (the pirate 🏴‍☠️).
function nameLine(node, { flag, name, wheel }) {
  node.textContent = "";
  if (wheel) node.append("🚗 ");
  const code = flagCode(flag);
  if (code) {
    const img = document.createElement("img");
    img.className = "flag-img";
    img.src = `/vendor/flags/${code.toLowerCase()}.svg`;
    img.alt = code;
    img.title = code;
    img.width = 16; img.height = 12;
    node.append(img, " ");
  } else if (flag) {
    node.append(`${flag} `); // pirate / non-country flag — emoji renders fine
  }
  node.append(name || "");
}

// Server time of a message as HH:MM (from its unix-seconds `ts`), in the
// viewer's local zone — everyone sees the same instant, formatted locally.
function fmtTime(ts) {
  if (!ts) return "";
  const d = new Date(ts * 1000);
  return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
}

// Is this message ours? By connection id (live) or by our display name (survives
// a reload, where a fresh connection has a new id but the same chosen name).
function isMine(m) {
  return (
    (m.cid != null && m.cid === state.partyCid) ||
    (!!m.from && !!state.partyName && m.from === state.partyName)
  );
}

// Flash the chat badge when a message lands and the panel is closed, so activity
// is noticeable. The CSS animation inverts its colours a couple of times.
function flashPartyBadge() {
  if (el.partyPanel.classList.contains("open")) return;
  playChatBlip(); // a light tick with the flash — activity you can hear
  el.partyBadge.classList.remove("party-blink");
  void el.partyBadge.offsetWidth; // restart the animation
  el.partyBadge.classList.add("party-blink");
}

function renderMsg({ from, flag, text, mine, ts }) {
  // The current driver's messages are marked (a 🚗 + accent name).
  const isDriver = !!from && from === state.partyDriver;
  const wrap = document.createElement("div");
  wrap.className = `party-msg${mine ? " mine" : ""}`;
  const who = document.createElement("div");
  who.className = `party-msg-from${isDriver ? " driver" : ""}`;
  nameLine(who, { flag, name: from, wheel: isDriver });
  const t = fmtTime(ts);
  if (t) {
    const time = document.createElement("span");
    time.className = "party-msg-time";
    time.textContent = t;
    who.append(time);
  }
  const body = document.createElement("div");
  body.className = "party-msg-text";
  body.textContent = text;
  wrap.append(who, body);
  // Observe follow-mode BEFORE appending (the append itself changes the
  // distance), then dock only if the reader was at the bottom. Our own sent
  // message always docks — you want to see what you just said.
  const wasPinned = partyFollow || mine;
  el.partyLog.appendChild(wrap);
  if (wasPinned) partyScrollToBottom();
}

function sendChat() {
  const text = el.partyInput.value.trim();
  if (!text) return;
  // The global hub echoes every message back to everyone (including us), so we
  // just send — our own copy arrives right back and renders (right-aligned by
  // cid). No local echo, no duplicate. If the socket is down, keep the text.
  if (!sendWs({ type: "party_chat", text })) {
    renderMsg({ from: "система", text: "Нет связи — сообщение не отправлено." });
    return;
  }
  el.partyInput.value = "";
  autoGrow();
}

// --- Join modal (guest picks a name) ------------------------------------------
function updateJoinHint() {
  const v = el.partyJoinName.value;
  if (!v) {
    el.partyJoinHint.textContent = "Оставишь пустым — будешь «Гость N».";
  } else if ([...v].length < 5) {
    el.partyJoinHint.textContent = "Ещё чуть-чуть — минимум 5 символов.";
  } else {
    el.partyJoinHint.textContent = `Отлично — «${v}».`;
  }
}

function submitJoin() {
  const v = sanitizeName(el.partyJoinName.value);
  // Empty is allowed (server auto-names); a typed name must be 5–10 characters.
  if (v && [...v].length < 5) {
    updateJoinHint();
    el.partyJoinName.focus();
    return;
  }
  storeName(v);
  el.partyJoinOverlay.hidden = true;
  sendIdentity();
}

function maybeShowJoin() {
  if (!state.gated || storedName()) return; // owner, or already named
  el.partyJoinName.value = "";
  updateJoinHint();
  el.partyJoinOverlay.hidden = false;
  el.partyJoinName.focus();
}

// --- Wiring -------------------------------------------------------------------
el.partyComposer.addEventListener("submit", (e) => {
  e.preventDefault();
  sendChat();
});
el.partyInput.addEventListener("input", autoGrow);
el.partyInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    sendChat();
  }
});
el.partyJoinName.addEventListener("input", () => {
  // Strip disallowed characters as they type, then refresh the hint.
  el.partyJoinName.value = sanitizeName(el.partyJoinName.value);
  updateJoinHint();
});
el.partyJoinName.addEventListener("keydown", (e) => {
  if (e.key === "Enter") {
    e.preventDefault();
    submitJoin();
  }
});
el.partyJoinGo.addEventListener("click", submitJoin);

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && el.partyPanel.classList.contains("open")) setPartyPanel(false);
});

window.addEventListener("cwi-party-open", () => {
  el.partyBadge.classList.remove("party-blink"); // looking at it now — stop nudging
  autoGrow();
  el.partyInput.focus();
  partyScrollToBottom(); // opening lands you on the newest — and re-arms follow
});

// Server → client frames (re-emitted by ws.js as DOM events).
window.addEventListener("cwi-open", sendIdentity);
window.addEventListener("cwi-gated", maybeShowJoin);
window.addEventListener("cwi-role", (e) => {
  const d = e.detail || {};
  state.partyRole = d.role === "observer" ? "observer" : "driver";
  if (d.name) state.partyName = d.name;
  applyRole();
});
window.addEventListener("cwi-roster", (e) => {
  const d = e.detail || {};
  updateDriverHead(d.members || []);
  updateOnlineCount(d.members || []);
});
// Our own connection id — messages carrying it are ours (right-aligned).
window.addEventListener("cwi-me", (e) => {
  const d = e.detail || {};
  if (d.cid != null) state.partyCid = d.cid;
  // The server may have numbered our name to keep it unique ("Антон" → "Антон 2")
  // — adopt what it actually assigned so our own messages are recognised.
  if (d.name) state.partyName = d.name;
});
window.addEventListener("cwi-party-chat", (e) => {
  const d = e.detail || {};
  const mine = isMine(d);
  renderMsg({ from: d.from || "?", flag: d.flag, text: d.text || "", mine, ts: d.ts });
  if (!mine) flashPartyBadge(); // someone else wrote → nudge the badge
});
// Global chat history on (re)connect — the authoritative last-N, so rebuild the
// log from it.
window.addEventListener("cwi-party-history", (e) => {
  const msgs = (e.detail && e.detail.messages) || [];
  el.partyLog.innerHTML = "";
  for (const m of msgs) {
    renderMsg({ from: m.from || "?", flag: m.flag, text: m.text || "", mine: isMine(m), ts: m.ts });
  }
  partyScrollToBottom(); // a fresh replay lands on the newest
});
