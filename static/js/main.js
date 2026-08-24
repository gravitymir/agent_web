// Entry point. Modules are loaded in dependency order (state is the leaf), then
// the app is bootstrapped once everything is defined.
import "./state.js";
import "./ws.js";
import "./render.js";
import "./ios-icons.js";
import { setFaviconState } from "../favicon.js";
import { init } from "./ui.js";
import "./guest.js"; // "Гостевой сервер" tab (executor VM control)
import "./links.js"; // "Ссылки" tab (guest magic-link management)
import "./files.js"; // "Файлы" drawer (read-only workspace explorer)
import "./party.js"; // "Комната" drawer (human side-chat + control hand-off)

setFaviconState("idle");
init();
