// "Файлы" drawer: a read-only explorer over the workspace, backed by
// /api/fs/list and /api/fs/read. Two views share the panel — the directory
// listing, and a single-file preview that replaces it (the column is too narrow
// to split). Paths are always workspace-relative strings; "" is the root, and
// the server decides whether a parent exists, so the sandbox edge lives in one
// place instead of being re-derived here.
//
// Admin-only, like the other two left drawers: the badge and panel carry
// `.admin-only[hidden]` and links.js un-hides them once /api/providers says this
// is the owner instance. On a guest instance the routes also answer 403.
import { el } from "./state.js";
import { setFilesDrawer } from "./ui.js";

// Current directory, workspace-relative. "" = workspace root.
let cwd = "";

function fmtSize(bytes) {
  if (bytes === null || bytes === undefined) return "";
  if (bytes < 1024) return `${bytes} Б`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} КБ`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} МБ`;
}

function fmtDate(iso) {
  if (!iso) return "";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "";
  return d.toLocaleString("ru-RU", {
    day: "2-digit",
    month: "2-digit",
    year: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function icon(kind) {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("fill", "none");
  svg.setAttribute("stroke", "currentColor");
  svg.setAttribute("stroke-width", "1.7");
  svg.setAttribute("stroke-linecap", "round");
  svg.setAttribute("stroke-linejoin", "round");
  svg.classList.add("files-icon");
  const p = document.createElementNS("http://www.w3.org/2000/svg", "path");
  p.setAttribute(
    "d",
    kind === "dir"
      ? "M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"
      : "M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8zM14 3v5h5",
  );
  svg.appendChild(p);
  return svg;
}

/** Join the current directory with a child name (no leading slash at the root). */
function childPath(name) {
  return cwd ? `${cwd}/${name}` : name;
}

function setMessage(text) {
  el.filesList.innerHTML = "";
  const d = document.createElement("div");
  d.className = "files-empty";
  d.textContent = text;
  el.filesList.appendChild(d);
}

/** Breadcrumb: root name plus one clickable chip per path segment. */
function renderPath(rootName, path) {
  el.filesPath.innerHTML = "";
  const crumb = (label, target) => {
    const b = document.createElement("button");
    b.type = "button";
    b.className = "files-crumb";
    b.textContent = label;
    b.addEventListener("click", () => load(target));
    return b;
  };
  el.filesPath.appendChild(crumb(rootName, ""));
  let acc = "";
  for (const seg of path ? path.split("/") : []) {
    acc = acc ? `${acc}/${seg}` : seg;
    const sep = document.createElement("span");
    sep.className = "files-crumb-sep";
    sep.textContent = "/";
    el.filesPath.append(sep, crumb(seg, acc));
  }
}

function showListing() {
  el.filesView.hidden = true;
  el.filesList.hidden = false;
}

/** Fetch and render one directory. */
export async function load(path) {
  let data;
  try {
    const r = await fetch(`/api/fs/list?path=${encodeURIComponent(path)}`);
    if (!r.ok) {
      setMessage(
        r.status === 403
          ? "Проводник доступен только на хосте владельца."
          : `Не удалось открыть папку (${r.status}).`,
      );
      showListing();
      return;
    }
    data = await r.json();
  } catch {
    setMessage("Ошибка сети.");
    showListing();
    return;
  }

  cwd = data.path;
  showListing();
  renderPath(data.root_name, data.path);
  // The server reports whether a parent exists; at the root there is none.
  el.filesUp.disabled = data.parent === null || data.parent === undefined;
  el.filesUp.dataset.parent = data.parent ?? "";

  el.filesList.innerHTML = "";
  if (!data.entries.length) {
    setMessage("Папка пуста.");
    return;
  }
  for (const entry of data.entries) {
    const row = document.createElement("button");
    row.type = "button";
    row.className = `files-row files-${entry.kind}`;
    row.appendChild(icon(entry.kind));

    const name = document.createElement("span");
    name.className = "files-name";
    name.textContent = entry.name;
    row.appendChild(name);

    const meta = document.createElement("span");
    meta.className = "files-meta";
    meta.textContent = entry.kind === "dir" ? "" : fmtSize(entry.size);
    row.appendChild(meta);

    row.title = `${entry.name}${entry.modified ? ` · ${fmtDate(entry.modified)}` : ""}`;
    row.addEventListener("click", () =>
      entry.kind === "dir" ? load(childPath(entry.name)) : openFile(childPath(entry.name)),
    );
    el.filesList.appendChild(row);
  }
  if (data.truncated) {
    const d = document.createElement("div");
    d.className = "files-empty";
    d.textContent = "Показаны не все файлы: слишком много элементов.";
    el.filesList.appendChild(d);
  }
}

/** Fetch a file and show it in the preview view. */
export async function openFile(path) {
  el.filesList.hidden = true;
  el.filesView.hidden = false;
  el.filesViewName.textContent = path.split("/").pop();
  el.filesViewMeta.textContent = "загрузка…";
  el.filesViewBody.textContent = "";

  let data;
  try {
    const r = await fetch(`/api/fs/read?path=${encodeURIComponent(path)}`);
    if (!r.ok) {
      el.filesViewMeta.textContent = `не удалось открыть (${r.status})`;
      return;
    }
    data = await r.json();
  } catch {
    el.filesViewMeta.textContent = "ошибка сети";
    return;
  }

  const bits = [fmtSize(data.size)];
  if (data.truncated) bits.push("показано начало файла");
  el.filesViewMeta.textContent = bits.join(" · ");
  // textContent, never innerHTML: file contents are untrusted markup.
  el.filesViewBody.textContent = data.binary
    ? "Двоичный файл — предпросмотр недоступен."
    : data.content;
  el.filesViewBody.classList.toggle("files-view-note", !!data.binary);
  el.filesViewBody.scrollTop = 0;
}

el.filesUp.addEventListener("click", () => {
  if (!el.filesUp.disabled) load(el.filesUp.dataset.parent || "");
});
el.filesRefresh.addEventListener("click", () => {
  // From the preview, refresh means "back to the listing and reload it".
  load(cwd);
});
el.filesBack.addEventListener("click", showListing);

// Escape: close the preview first, then the drawer — the usual shell order.
document.addEventListener("keydown", (e) => {
  if (e.key !== "Escape" || !el.filesDrawer.classList.contains("open")) return;
  if (!el.filesView.hidden) showListing();
  else setFilesDrawer(false);
});

// Opening the drawer re-reads the current directory every time — so closing and
// reopening the explorer is itself the "refresh" gesture. The last directory is
// kept (not reset to root), just re-fetched. While it stays open nothing polls;
// the ⟳ button at the top is the only other way to refresh.
window.addEventListener("cwi-files-open", () => {
  load(cwd);
});
