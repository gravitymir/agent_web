// Desktop notification when the assistant finishes a turn while the tab is
// hidden — the audible chime alone doesn't help if the user stepped away,
// switched tabs, or muted the tab.

// Requesting permission must happen on a user gesture (a checkbox click
// qualifies); call this from that handler, not proactively on load. Resolves
// with the resulting Notification.permission ("granted"/"denied"/"default") —
// browsers refuse to re-prompt once denied, and a dismissed prompt can leave
// it "default" forever, so the caller needs this to tell the user why nothing
// will actually show, instead of the checkbox looking "on" for nothing.
export function ensureNotifyPermission() {
  if (!("Notification" in window)) return Promise.resolve("unsupported");
  if (Notification.permission !== "default") return Promise.resolve(Notification.permission);
  return Notification.requestPermission();
}

// Plain-text snippet of the answer for the notification body — a native OS
// notification can't render markdown, so code fences (rarely meaningful out of
// context anyway) are dropped rather than shown as raw ``` noise.
function buildPreview(text, max = 100) {
  const clean = (text || "")
    .replace(/```[\s\S]*?```/g, " ")
    .replace(/\s+/g, " ")
    .trim();
  if (!clean) return "";
  return clean.length > max ? clean.slice(0, max - 1) + "…" : clean;
}

export function notifyTurnComplete(chatTitle, answerText) {
  if (!("Notification" in window)) return;
  if (Notification.permission !== "granted") return;
  // `document.hidden` alone only catches switching to a DIFFERENT TAB in the
  // same window — if this tab is still the active one but the whole browser
  // window lost OS focus (switched to another app), it stays `false` and the
  // notification would never fire even though the user isn't looking at it.
  // `hasFocus()` catches that case too.
  const activelyViewed = document.hasFocus() && !document.hidden;
  if (activelyViewed) return;
  // Chat name on its own line, then a snippet of what the answer actually
  // said — more useful than the title alone for deciding whether to switch.
  const body = [chatTitle, buildPreview(answerText)].filter(Boolean).join("\n")
    || "Чат готов к следующему сообщению";
  try {
    const n = new Notification("Ответ готов", {
      body,
      // No `icon` — the orange square added no information, just visual noise.
      // A fixed tag would make each turn REPLACE the previous notification —
      // on Windows that silently updates the Action Center entry without
      // re-showing the toast banner, so only the very first one in a session
      // was ever actually seen. Unique per call so every turn gets its own toast.
      tag: `cwi-turn-complete-${Date.now()}`,
    });
    n.onclick = () => {
      window.focus();
      n.close();
    };
  } catch (e) {
    // Notification construction can throw on some platforms — non-fatal.
  }
}
