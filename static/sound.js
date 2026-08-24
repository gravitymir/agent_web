// A short, soft chime played when the assistant's turn finishes, so the user
// doesn't have to watch the tab to know the chat is ready for the next message.

let ctx = null;

function getContext() {
  if (!ctx) ctx = new (window.AudioContext || window.webkitAudioContext)();
  // Suspended until a user gesture unlocks it (autoplay policy); sending the
  // message that led to this turn already counts as one, so this is best-effort.
  if (ctx.state === "suspended") ctx.resume().catch(() => {});
  return ctx;
}

function tone(ac, freq, startAt, duration, peakGain) {
  const osc = ac.createOscillator();
  const gain = ac.createGain();
  osc.type = "sine";
  osc.frequency.value = freq;
  gain.gain.setValueAtTime(0, startAt);
  gain.gain.linearRampToValueAtTime(peakGain, startAt + 0.015); // soft attack, no click
  gain.gain.exponentialRampToValueAtTime(0.0001, startAt + duration);
  osc.connect(gain).connect(ac.destination);
  osc.start(startAt);
  osc.stop(startAt + duration + 0.02);
}

// Two quiet ascending notes — deliberately subtle, not an alert sound.
export function playCompletionChime() {
  try {
    const ac = getContext();
    const now = ac.currentTime;
    tone(ac, 880.0, now, 0.16, 0.12); // A5
    tone(ac, 1318.5, now + 0.1, 0.22, 0.1); // E6
  } catch (e) {
    // Web Audio unavailable/blocked — silently skip, this is a nice-to-have.
  }
}

// Tiny cues for toggling voice dictation — deliberately short and distinct so
// the two are told apart with eyes off the screen: a RISING pair when listening
// starts, a FALLING pair when it stops.
export function playDictationStart() {
  try {
    const ac = getContext();
    const now = ac.currentTime;
    tone(ac, 587.33, now, 0.08, 0.11); // D5
    tone(ac, 987.77, now + 0.07, 0.11, 0.11); // B5 (rising = on)
  } catch (e) {
    // best-effort
  }
}

export function playDictationStop() {
  try {
    const ac = getContext();
    const now = ac.currentTime;
    tone(ac, 987.77, now, 0.08, 0.1); // B5
    tone(ac, 587.33, now + 0.07, 0.11, 0.1); // D5 (falling = off)
  } catch (e) {
    // best-effort
  }
}

// A single light, short "blip" for an incoming room-chat message while the chat
// panel is closed (played alongside the badge flash). Quieter and briefer than
// the completion chime on purpose — chat can be chatty — and a different pitch
// so it's told apart from the turn-done and dictation cues by ear.
export function playChatBlip() {
  try {
    const ac = getContext();
    const now = ac.currentTime;
    tone(ac, 1174.66, now, 0.07, 0.07); // D6 — one soft tick
  } catch (e) {
    // best-effort
  }
}
