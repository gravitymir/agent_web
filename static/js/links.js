// "Ссылки" admin tab: mint / list / revoke guest magic links from the master
// page. Admin-only — the server reports `admin` in /api/providers; on a guest
// instance (CWI_AUTH on) the tab stays hidden and the routes return 403. Codes
// are validated on the disposable executor; the host pushes them there on mint.
const $ = (id) => document.getElementById(id);

function humanTtl(s) {
  if (s >= 86400) return `${Math.floor(s / 86400)}д`;
  if (s >= 3600) return `${Math.floor(s / 3600)}ч`;
  if (s >= 60) return `${Math.floor(s / 60)}м`;
  return `${s}с`;
}

async function loadList() {
  const ul = $("links-list");
  if (!ul) return;
  let items = [];
  try {
    const r = await fetch("/api/links");
    if (!r.ok) return;
    items = await r.json();
  } catch {
    return;
  }
  ul.innerHTML = "";
  if (!items.length) {
    const li = document.createElement("li");
    li.className = "links-empty";
    li.textContent = "Активных ссылок нет.";
    ul.appendChild(li);
    return;
  }
  for (const it of items) {
    const li = document.createElement("li");
    li.className = "links-row";
    const info = document.createElement("span");
    info.className = "links-row-info";
    info.textContent = `${it.label} · ещё ${humanTtl(it.expires_in)}`;
    const btn = document.createElement("button");
    btn.className = "link-revoke";
    btn.textContent = "Отозвать";
    btn.addEventListener("click", async () => {
      btn.disabled = true;
      try {
        await fetch(`/api/links/${encodeURIComponent(it.label)}`, { method: "DELETE" });
      } catch {}
      loadList();
    });
    li.append(info, btn);
    ul.appendChild(li);
  }
}

function showResult(text, copyable) {
  const box = $("link-result");
  if (!box) return;
  box.hidden = false;
  box.innerHTML = "";
  const url = document.createElement("div");
  url.className = "link-url";
  url.textContent = text;
  box.appendChild(url);
  if (copyable) {
    const c = document.createElement("button");
    c.className = "btn-secondary guest-btn link-copy";
    c.textContent = "Скопировать";
    c.addEventListener("click", async () => {
      try {
        await navigator.clipboard.writeText(copyable);
        c.textContent = "Скопировано ✓";
      } catch {
        // clipboard may be blocked (insecure origin) — select-and-copy fallback
        const r = document.createRange();
        r.selectNodeContents(url);
        const sel = window.getSelection();
        sel.removeAllRanges();
        sel.addRange(r);
      }
    });
    box.appendChild(c);
  }
}

async function create() {
  const labelEl = $("link-label");
  const label = labelEl.value.trim();
  if (!label) {
    labelEl.focus();
    return;
  }
  const ttl = $("link-ttl").value;
  const btn = $("link-create");
  btn.disabled = true;
  try {
    const r = await fetch("/api/links", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ label, ttl }),
    });
    if (!r.ok) {
      showResult("Не удалось создать ссылку.", null);
      return;
    }
    const d = await r.json();
    showResult(d.magic_link, d.magic_link);
    labelEl.value = "";
    loadList();
  } catch {
    showResult("Ошибка сети.", null);
  } finally {
    btn.disabled = false;
  }
}

async function init() {
  let admin = false;
  try {
    const r = await fetch("/api/providers");
    if (r.ok) admin = !!(await r.json()).admin;
  } catch {}
  if (!admin) return; // guest instance — leave the admin badge/drawer hidden
  document
    .querySelectorAll(".admin-only[hidden]")
    .forEach((el) => el.removeAttribute("hidden"));
  $("link-create")?.addEventListener("click", create);
  $("link-label")?.addEventListener("keydown", (e) => {
    if (e.key === "Enter") create();
  });
  // Reload the list each time the admin drawer opens (dispatched by ui.js).
  window.addEventListener("cwi-admin-open", loadList);
}

init();
