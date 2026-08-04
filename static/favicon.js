// Dynamic favicon / tab icon based on the open chat's state.
// Draws the Agentron mark (cream) on a rounded-square badge whose colour signals
// the response mode; the mark spins while the agent is active so the tab reads as
// "alive" at a glance. Server still serves /favicon.svg as the static fallback.

const canvasSize = 32;
let faviconLink = null;
let animationId = null;
let lastState = null;
let phase = 0;

// Per-state look: badge colour + how fast the clock hands turn (rad per ms;
// 0 = still). idle rests (done/ready); the working states tick, tool also pulses.
// Badge colours are deliberately deep so the white mark hits high contrast on
// every state (WCAG contrast vs #fff, all >= ~4.5): green 5.3, amber 4.7, red
// 6.7, blue 7.1 — maximum separation between the square and the logo.
const STATES = {
  idle:     { bg: "#0b7d3d", spin: 0,      pulse: false },
  thinking: { bg: "#c0521a", spin: 0.0060, pulse: false },
  tool:     { bg: "#b02418", spin: 0.0050, pulse: true  },
  output:   { bg: "#1857a8", spin: 0.0035, pulse: false },
};
const MARK = "#ffffff"; // pure white mark — highest contrast on every badge colour

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

function roundRect(ctx, x, y, w, h, r) {
  ctx.beginPath();
  if (ctx.roundRect) {
    ctx.roundRect(x, y, w, h, r);
  } else {
    ctx.moveTo(x + r, y);
    ctx.arcTo(x + w, y, x + w, y + h, r);
    ctx.arcTo(x + w, y + h, x, y + h, r);
    ctx.arcTo(x, y + h, x, y, r);
    ctx.arcTo(x, y, x + w, y, r);
    ctx.closePath();
  }
}

const FACE = { x: 11, y: 13, r: 7 };   // clock face centre + radius (24-unit space)
const THETA_REST = 0.74;               // parked antenna leans up-right (the brand pose)

// The antenna orbits the skull in its own rhythm — mostly counter-clockwise, but
// speeding up, easing off, pausing, and now and then nudging the other way. `phase`
// is elapsed ms. Over one period it makes exactly ONE net counter-clockwise turn,
// and the wiggle terms vanish at the seam, so theta(0) === theta(P) (mod 2pi):
// the loop is joinless. Starts from THETA_REST so it blends out of the idle pose.
function antennaAngle(phase) {
  const TAU = Math.PI * 2;
  const P = 11000;                          // one leisurely orbit
  const u = (phase % P) / P;                // 0..1
  // -TAU*u = one CCW revolution; the sines are the rhythm (zero at u=0 and u=1).
  const wiggle = 0.9 * Math.sin(TAU * u) + 0.5 * Math.sin(2 * TAU * u) + 0.25 * Math.sin(3 * TAU * u);
  return THETA_REST - TAU * u - wiggle;
}

// The Agentron mark in its native 24x24 coordinate space — same geometry as the
// SVG in ios-icons.js. The head + ears are fixed; the antenna orbits the skull at
// angle `theta` (0 = straight up, + = right); the hands turn independently (minute
// 60x the hour) so the clock reads as realistically running.
function traceMark(ctx, minuteRad, hourRad, theta) {
  const TAU = Math.PI * 2;
  const s = Math.sin(theta), c = Math.cos(theta);
  const at = (dist) => [FACE.x + dist * s, FACE.y - dist * c];   // point up the antenna axis
  const base = at(FACE.r + 0.05), tip = at(FACE.r + 4.05);

  ctx.beginPath(); ctx.arc(FACE.x, FACE.y, FACE.r, 0, TAU); ctx.stroke();  // head
  ctx.beginPath(); ctx.ellipse(4.5, 13, 1.4, 2.5, 0, Math.PI / 2, Math.PI * 1.5); ctx.stroke();  // left ear
  ctx.beginPath(); ctx.ellipse(17.5, 13, 1.4, 2.5, 0, -Math.PI / 2, Math.PI / 2); ctx.stroke();  // right ear

  ctx.beginPath(); ctx.moveTo(base[0], base[1]);                           // antenna stalk
  ctx.lineTo(FACE.x + (FACE.r + 2.2) * s, FACE.y - (FACE.r + 2.2) * c); ctx.stroke();
  ctx.beginPath(); ctx.arc(tip[0], tip[1], 1.8, 0, TAU); ctx.stroke();     // antenna tip

  ctx.save();
  ctx.translate(FACE.x, FACE.y);
  ctx.rotate(hourRad);
  ctx.beginPath(); ctx.moveTo(0, 0); ctx.lineTo(0, -4); ctx.stroke();      // hour hand
  ctx.restore();
  ctx.save();
  ctx.translate(FACE.x, FACE.y);
  ctx.rotate(minuteRad);
  ctx.beginPath(); ctx.moveTo(0, 0); ctx.lineTo(2.7, 1.6); ctx.stroke();   // minute hand
  ctx.restore();
}

// Centred on the clock FACE so the orbiting antenna sweeps symmetrically. The
// bound is the antenna tip's outer edge at its farthest reach (face + 4.05 stalk
// + 1.8 tip + ~1.05 stroke ≈ 13.9 units): 13.9 * s <= ~15.3 keeps ~0.7px clear of
// the 16px canvas radius at every orbit angle. The head fills the middle; the
// state colour shows as a ring around it, and the antenna reaches the edges.
const MARK_SCALE = 1.1;

function drawIcon(ctx, cfg, phase) {
  const c = canvasSize / 2;
  ctx.clearRect(0, 0, canvasSize, canvasSize);

  // Badge: rounded square in the state colour, gently breathing for "tool".
  const pulseMs = cfg.pulse ? phase : 0;
  const size = 31 + (pulseMs ? Math.sin(pulseMs * 0.008) * 1.6 : 0);
  const half = size / 2;
  ctx.fillStyle = cfg.bg;
  roundRect(ctx, c - half, c - half, size, size, size * 0.26);
  ctx.fill();

  // Motion: minute hand at cfg.spin, hour hand 60x slower (a real running clock);
  // antenna in its own orbiting rhythm. All still when the state is at rest.
  const minuteRad = phase * cfg.spin;
  const hourRad = minuteRad / 60;
  const theta = cfg.spin ? antennaAngle(phase) : THETA_REST;

  ctx.save();
  ctx.translate(c, c);
  ctx.scale(MARK_SCALE, MARK_SCALE);
  ctx.translate(-FACE.x, -FACE.y);
  ctx.strokeStyle = MARK;
  ctx.lineWidth = 2.1;
  ctx.lineCap = "round";
  ctx.lineJoin = "round";
  traceMark(ctx, minuteRad, hourRad, theta);
  ctx.restore();
}

function makeIcon(type, phase = 0) {
  const canvas = document.createElement("canvas");
  canvas.width = canvasSize;
  canvas.height = canvasSize;
  const ctx = canvas.getContext("2d");
  drawIcon(ctx, STATES[type] || STATES.idle, phase);
  return canvas.toDataURL("image/png");
}

export function setFaviconState(type) {
  lastState = type;
  phase = 0;
  if (animationId) {
    cancelAnimationFrame(animationId);
    animationId = null;
  }

  const cfg = STATES[type] || STATES.idle;

  if (cfg.spin || cfg.pulse) {
    let last = performance.now();
    const tick = (now) => {
      phase += now - last;
      last = now;
      setFavicon(makeIcon(type, phase));
      animationId = requestAnimationFrame(tick);
    };
    setFavicon(makeIcon(type, 0));
    animationId = requestAnimationFrame(tick);
  } else {
    setFavicon(makeIcon(type, 0));
  }
}

export function stopFaviconAnimation() {
  if (animationId) {
    cancelAnimationFrame(animationId);
    animationId = null;
  }
}
