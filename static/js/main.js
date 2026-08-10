// Entry point. Modules are loaded in dependency order (state is the leaf), then
// the app is bootstrapped once everything is defined.
import "./state.js";
import "./ws.js";
import "./render.js";
import "./ios-icons.js";
import { setFaviconState } from "../favicon.js";
import { init } from "./ui.js";
import "./guest.js"; // "Гостевой сервер" tab (executor VM control)

setFaviconState("idle");
init();
