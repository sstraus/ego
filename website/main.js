// εgo website behaviour. No dependencies. The page is complete without it:
// every demo is written in its final state, and this script only replays the
// sequence that leads there. Under prefers-reduced-motion nothing moves.

const still = matchMedia("(prefers-reduced-motion: reduce)").matches;
const $ = (s, el = document) => el.querySelector(s);
const $$ = (s, el = document) => [...el.querySelectorAll(s)];

/* Navigation condenses after scrolling */
const nav = $(".nav");
const onScroll = () => nav.classList.toggle("scrolled", scrollY > 8);
addEventListener("scroll", onScroll, { passive: true });
onScroll();

/* Copy buttons */
for (const button of $$("[data-copy]")) {
  button.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(button.dataset.copy);
      button.textContent = "Copied";
    } catch {
      button.textContent = "Select and copy";
    }
    setTimeout(() => (button.textContent = "Copy"), 1600);
  });
}

/* Demos: [data-at="ms"] turns on, [data-off="ms"] folds away,
   [data-count="from to"] ticks a M:SS clock down over data-count-ms. */
const clock = (s) => `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;

function settle(demo) {
  for (const el of $$("[data-at]", demo)) el.classList.add("on");
  for (const el of $$("[data-off]", demo)) el.classList.add("off");
  for (const el of $$("[data-count]", demo)) el.textContent = clock(Number(el.dataset.count.split(" ")[1]));
}

function play(demo) {
  for (const t of demo._timers || []) clearTimeout(t);
  cancelAnimationFrame(demo._raf);
  const timers = (demo._timers = []);
  const at = (ms, fn) => timers.push(setTimeout(fn, ms));
  for (const el of $$("[data-at]", demo)) el.classList.remove("on");
  for (const el of $$("[data-off]", demo)) el.classList.remove("off");
  void demo.offsetWidth; // restart transitions from the initial state
  for (const el of $$("[data-at]", demo)) at(Number(el.dataset.at), () => el.classList.add("on"));
  for (const el of $$("[data-off]", demo)) at(Number(el.dataset.off), () => el.classList.add("off"));
  for (const el of $$("[data-count]", demo)) {
    const [from, to] = el.dataset.count.split(" ").map(Number);
    const ms = Number(el.dataset.countMs || 2000);
    el.textContent = clock(from);
    at(Number(el.dataset.at || 0), () => {
      const t0 = performance.now();
      const tick = (t) => {
        const k = Math.min(1, (t - t0) / ms);
        el.textContent = clock(Math.round(from - (from - to) * k));
        if (k < 1) demo._raf = requestAnimationFrame(tick);
      };
      demo._raf = requestAnimationFrame(tick);
    });
    // Land on the final value even where animation frames are paused.
    at(Number(el.dataset.at || 0) + ms + 50, () => (el.textContent = clock(to)));
  }
}

for (const demo of $$("[data-demo]")) {
  const replay = $(".replay", demo);
  if (still) {
    settle(demo);
    if (replay) replay.hidden = true;
    continue;
  }
  replay?.addEventListener("click", () => play(demo));
  if (demo.dataset.demo === "hero") {
    play(demo);
    continue;
  }
  // Hide the sequence until it is first seen, then play it once.
  for (const el of $$("[data-at]", demo)) el.classList.remove("on");
  const io = new IntersectionObserver(
    (entries) => {
      if (!entries.some((e) => e.isIntersecting)) return;
      io.disconnect();
      play(demo);
    },
    { threshold: 0.35 },
  );
  io.observe(demo);
}
