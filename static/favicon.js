// Dynamic favicon / tab icon: the Agentron stopwatch itself — no background
// square — recoloured to signal what the agent is doing right now. The mark is
// asymmetric so it doesn't spin; instead the whole stopwatch changes colour per
// event. The ring gap, clock hands and crown hole are punched to transparency,
// so on a normal tab the mark reads exactly like the logo. Server still serves
// /favicon.svg as the static fallback.

const canvasSize = 32;
let faviconLink = null;

// Colour per event. Green = the agent finished (idle/ready). The working states
// each get their own hue so a glance at the tab tells you the current activity:
//   thinking  — reasoning            (purple)
//   read      — reading files        (turquoise)
//   search    — grep / glob / web    (yellow)
//   write     — writing / editing    (lime)
//   tool      — bash / other tools   (orange)
//   output    — streaming the answer (light blue)
const STATES = {
  idle:     "#16a34a", // green  — done / ready
  thinking: "#8b5cf6", // purple
  read:     "#14b8a6", // turquoise
  search:   "#eab308", // yellow
  write:    "#84cc16", // lime
  tool:     "#f97316", // orange
  output:   "#38bdf8", // light blue
};

function getFaviconLink() {
  if (faviconLink) return faviconLink;
  faviconLink = document.getElementById("favicon-link");
  if (!faviconLink) faviconLink = document.querySelector('link[rel="icon"]');
  return faviconLink;
}

function setFavicon(href) {
  const link = getFaviconLink();
  if (!link) return;
  if (link.href === href) return;
  link.href = href;
}

// The stopwatch mark in a 0..100 space — same geometry as the SVG logo. Body,
// ears and crown are filled in the state colour; the ring gap, hands and crown
// hole are then erased (destination-out) so they are transparent cutouts.
const BODY = { x: 48.75, y: 58.69, r: 34.75 };
const GAP  = { r: 26.42, w: 4.6 };
const DONUT = { x: 85.52, y: 17.04, r: 10.47, hole: 5.24 };
const HAND_HOUR = { x: 48.75, y: 42.27 };   // tip at 12
const HAND_MIN  = { x: 63.03, y: 71.54 };    // tip at ~4:30

function drawMark(ctx, color) {
  const TAU = Math.PI * 2;

  // Solid stopwatch in the state colour.
  ctx.fillStyle = color;
  ctx.strokeStyle = color;
  ctx.lineCap = "round";
  ctx.lineJoin = "round";
  ctx.beginPath(); ctx.ellipse(13.65, 61.3, 9.88, 15.11, 0, 0, TAU); ctx.fill();   // left button
  ctx.beginPath(); ctx.ellipse(83.85, 61.3, 9.88, 15.11, 0, 0, TAU); ctx.fill();   // right button
  ctx.lineWidth = 6.43;                                                            // crown stem
  ctx.beginPath(); ctx.moveTo(70.65, 33.22); ctx.lineTo(78.74, 25.25); ctx.stroke();
  ctx.beginPath(); ctx.arc(BODY.x, BODY.y, BODY.r, 0, TAU); ctx.fill();            // face disc
  ctx.beginPath(); ctx.arc(DONUT.x, DONUT.y, DONUT.r, 0, TAU); ctx.fill();         // crown ring

  // Erase the negative space to transparency.
  ctx.globalCompositeOperation = "destination-out";
  ctx.strokeStyle = "#000";
  ctx.fillStyle = "#000";
  ctx.lineWidth = GAP.w;
  ctx.beginPath(); ctx.arc(BODY.x, BODY.y, GAP.r, 0, TAU); ctx.stroke();           // ring gap
  ctx.beginPath(); ctx.arc(DONUT.x, DONUT.y, DONUT.hole, 0, TAU); ctx.fill();      // crown hole
  ctx.lineWidth = 6;
  ctx.beginPath();                                                                 // hands
  ctx.moveTo(HAND_HOUR.x, HAND_HOUR.y);
  ctx.lineTo(BODY.x, BODY.y);
  ctx.lineTo(HAND_MIN.x, HAND_MIN.y);
  ctx.stroke();
  ctx.globalCompositeOperation = "source-over";
}

// No square, so the mark can fill the frame: fit the 0..100 art (centred on
// ~50,50) to ~31px of the 32px canvas.
const MARK_SCALE = 0.33;

function makeIcon(color) {
  const canvas = document.createElement("canvas");
  canvas.width = canvasSize;
  canvas.height = canvasSize;
  const ctx = canvas.getContext("2d");
  ctx.clearRect(0, 0, canvasSize, canvasSize);
  const c = canvasSize / 2;
  ctx.save();
  ctx.translate(c, c);
  ctx.scale(MARK_SCALE, MARK_SCALE);
  ctx.translate(-50, -50);
  drawMark(ctx, color);
  ctx.restore();
  return canvas.toDataURL("image/png");
}

export function setFaviconState(type) {
  setFavicon(makeIcon(STATES[type] || STATES.idle));
}

// Kept for API compatibility; the favicon no longer animates.
export function stopFaviconAnimation() {}
