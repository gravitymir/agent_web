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

    // QR expander: a chevron + "QR" toggle that reveals a scannable code of the
    // link, so a guest can point a phone camera at the owner's screen instead of
    // typing or forwarding. The image is fetched lazily on first open.
    const qrToggle = document.createElement("button");
    qrToggle.type = "button";
    qrToggle.className = "btn-secondary guest-btn link-qr-toggle";
    const chev = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    chev.setAttribute("viewBox", "0 0 24 24");
    chev.setAttribute("fill", "none");
    chev.setAttribute("stroke", "currentColor");
    chev.setAttribute("stroke-width", "2.2");
    chev.setAttribute("stroke-linecap", "round");
    chev.setAttribute("stroke-linejoin", "round");
    chev.classList.add("qr-chevron");
    const chevPath = document.createElementNS("http://www.w3.org/2000/svg", "path");
    chevPath.setAttribute("d", "M6 9l6 6 6-6");
    chev.appendChild(chevPath);
    const qrLabel = document.createElement("span");
    qrLabel.textContent = "QR";
    qrToggle.append(chev, qrLabel);

    const qrBox = document.createElement("div");
    qrBox.className = "link-qr";
    qrBox.hidden = true;

    qrToggle.addEventListener("click", () => {
      const show = qrBox.hidden;
      qrBox.hidden = !show;
      qrToggle.classList.toggle("open", show);
      if (show && !qrBox.dataset.loaded) {
        const img = document.createElement("img");
        img.className = "link-qr-img";
        img.alt = "QR-код ссылки";
        img.src = `/api/links/qr?data=${encodeURIComponent(copyable)}`;
        qrBox.appendChild(img);
        qrBox.dataset.loaded = "1";
      }
    });
    box.append(qrToggle, qrBox);
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
