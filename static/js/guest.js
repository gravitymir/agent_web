// "Гостевой сервер" sidebar tab: control the disposable executor VM (start /
// drain-stop / stop) and show live status streamed over the WS as
// {"cwi":"executor",…} frames. ws.js re-emits those as a "cwi-executor" DOM
// event (avoids an import cycle); we send commands via sendWs.
import { sendWs } from "./ws.js";

const $ = (id) => document.getElementById(id);

const STATE_LABEL = {
  absent: "VM не найдена",
  stopped: "Остановлена",
  booting: "Загружается…",
  ready: "Работает",
  draining: "Завершение сессий…",
  stopping: "Останавливается…",
  error: "Ошибка",
  unknown: "…",
};

function logLine(text) {
  const log = $("guest-log");
  if (!log) return;
  const line = document.createElement("div");
  line.className = "guest-log-line";
  line.textContent = text;
  log.appendChild(line);
  while (log.children.length > 40) log.removeChild(log.firstChild);
  log.scrollTop = log.scrollHeight;
}

function setBusy(busy) {
  document.querySelectorAll(".guest-btn").forEach((b) => (b.disabled = busy));
}

function cmd(action) {
  logLine(`→ ${action}`);
  setBusy(true);
  sendWs({ type: "executor", action });
}

// One {"cwi":"executor",…} frame from the server.
function handleExecutorEvent(evt) {
  const st = evt.state || "unknown";
  const dot = $("guest-dot");
  if (dot) dot.dataset.state = st;
  const label = $("guest-state");
  if (label) label.textContent = STATE_LABEL[st] || st;

  if (evt.progress) logLine(evt.progress);

  const meta = $("guest-meta");
  if (meta) {
    const parts = [];
    if (evt.active_turns != null) parts.push(`агентов активно: ${evt.active_turns}`);
    if (evt.vm) {
      parts.push(evt.vm.running ? "VM работает" : "VM выключена");
      if (evt.vm.running && evt.vm.ssh_ready) parts.push("SSH готов");
      if (!evt.vm.clean_snapshot) parts.push("нет снапшота clean");
    }
    // End-to-end public reachability (through Cloudflare). VM/SSH can be green
    // while guests still get Bad Gateway if :8790 or the tunnel is down.
    if (evt.public) {
      parts.push(
        evt.public.reachable
          ? "публичный доступ ✓"
          : "публичный доступ ✗ — Bad Gateway (гость :8790 или туннель Cloudflare не отвечает; запустите run-guest.bat)",
      );
    }
    meta.textContent = parts.join(" · ");
    // Colour the whole meta line red when the public URL is unreachable — the
    // one condition an admin most needs to notice at a glance.
    meta.style.color = evt.public && !evt.public.reachable ? "var(--danger, #c0392b)" : "";
  }

  // Transient states keep buttons disabled; settled states re-enable per state.
  const settled = ["ready", "stopped", "absent", "error"].includes(st);
  if (!settled) {
    setBusy(true);
    return;
  }
  const running = st === "ready";
  if ($("guest-start")) $("guest-start").disabled = running || st === "absent";
  if ($("guest-drain")) $("guest-drain").disabled = !running;
  if ($("guest-stop")) $("guest-stop").disabled = !running;
}

function init() {
  $("guest-start")?.addEventListener("click", () => cmd("start"));
  $("guest-drain")?.addEventListener("click", () => cmd("drain"));
  $("guest-stop")?.addEventListener("click", () => cmd("stop"));
  window.addEventListener("cwi-executor", (e) => handleExecutorEvent(e.detail));
  // Guest + Links moved out of the sidebar tabs into their own admin drawer;
  // refresh VM status each time that drawer opens.
  window.addEventListener("cwi-admin-open", () => sendWs({ type: "executor", action: "status" }));
}
init();
