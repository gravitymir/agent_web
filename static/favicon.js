// Dynamic favicon / tab icon based on the open chat's state.
// Server still serves /favicon.svg as fallback for bookmarks and external links.

const canvasSize = 32;
let faviconLink = null;
let animationId = null;
let lastState = null;
let phase = 0;

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

// A rounded square, centered, optionally rotated (for the "thinking" spin) and
// gently breathing in size (for the "tool" pulse).
function drawSquare(ctx, color, rotation = 0, pulse = 0) {
  const cx = canvasSize / 2;
  const cy = canvasSize / 2;

  ctx.clearRect(0, 0, canvasSize, canvasSize);

  ctx.save();
  ctx.translate(cx, cy);
  ctx.rotate((rotation * Math.PI) / 180);

  const base = 28; // fill most of the 32px canvas so the tab icon reads clearly
  const size = pulse ? base + Math.sin(pulse * 0.008) * 2 : base;
  const half = size / 2;
  const r = size * 0.24; // corner radius ≈ 24%

  ctx.fillStyle = color;
  ctx.beginPath();
  if (ctx.roundRect) {
    ctx.roundRect(-half, -half, size, size, r);
  } else {
    ctx.moveTo(-half + r, -half);
    ctx.arcTo(half, -half, half, half, r);
    ctx.arcTo(half, half, -half, half, r);
    ctx.arcTo(-half, half, -half, -half, r);
    ctx.arcTo(-half, -half, half, -half, r);
    ctx.closePath();
  }
  ctx.fill();

  ctx.restore();
}

function makeIcon(type, rotation = 0, pulse = 0) {
  const canvas = document.createElement("canvas");
  canvas.width = canvasSize;
  canvas.height = canvasSize;
  const ctx = canvas.getContext("2d");

  switch (type) {
    case "thinking":
      drawSquare(ctx, "#1a1a1a", rotation, 0);
      break;
    case "tool":
      drawSquare(ctx, "#c0392b", 0, pulse);
      break;
    case "output":
      drawSquare(ctx, "#2e86de", 0, 0);
      break;
    case "idle":
    default:
      drawSquare(ctx, "#2ecc71", 0, 0);
      break;
  }

  return canvas.toDataURL("image/png");
}

export function setFaviconState(type) {
  lastState = type;
  phase = 0;
  if (animationId) {
    cancelAnimationFrame(animationId);
    animationId = null;
  }

  if (type === "thinking") {
    let last = performance.now();
    const tick = (now) => {
      phase += now - last;
      last = now;
      setFavicon(makeIcon("thinking", phase * 0.12));
      animationId = requestAnimationFrame(tick);
    };
    setFavicon(makeIcon("thinking", 0));
    animationId = requestAnimationFrame(tick);
  } else if (type === "tool") {
    let last = performance.now();
    const tick = (now) => {
      phase += now - last;
      last = now;
      setFavicon(makeIcon("tool", 0, phase));
      animationId = requestAnimationFrame(tick);
    };
    setFavicon(makeIcon("tool", 0, 0));
    animationId = requestAnimationFrame(tick);
  } else {
    setFavicon(makeIcon(type, 0, 0));
  }
}

export function stopFaviconAnimation() {
  if (animationId) {
    cancelAnimationFrame(animationId);
    animationId = null;
  }
}
