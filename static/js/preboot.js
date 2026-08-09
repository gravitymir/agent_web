// Apply the saved font size before first paint to avoid a flash. Loaded as a
// blocking <script src> in <head> (not deferred) so it runs before the body
// renders — externalized (rather than inline) so the CSP can drop
// script-src 'unsafe-inline'.
(function () {
  var f = localStorage.getItem("cwi_fontsize");
  if (f) document.documentElement.style.fontSize = f + "px";
})();
