// Compares when the same frame becomes visible through the media element versus
// through the WebCodecs overlay canvas. Both surfaces are sampled in the same
// rAF loop, so the difference between them is clock consistent; a positive
// delta means the canvas showed the change earlier.
import { chromium } from "playwright-core";
import { execFileSync } from "node:child_process";

const BASE = process.env.BASE ?? "http://127.0.0.1:8813";
const USER = "swayrtctest";
const PASS = process.env.USER_PASS;
if (!PASS) { console.error("USER_PASS is not set (test account password: see .secrets/test-password)"); process.exit(1); }
const ROUNDS = Number(process.env.ROUNDS ?? 8);
const WALL = process.env.WALL ?? "/home/s0u7a/ダウンロード/copland_os_lain_by_phate1128_dxxpea.jpg";

const browser = await chromium.launch({
  executablePath: "/usr/bin/google-chrome",
  headless: true,
  args: ["--no-sandbox", "--autoplay-policy=no-user-gesture-required"],
});
const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
await page.goto(BASE + "/", { waitUntil: "domcontentloaded" });
await page.evaluate(() => { try { sessionStorage.setItem("swayrtc.renderer", "webcodecs"); } catch { /* blocked */ } });
await page.reload({ waitUntil: "domcontentloaded" });
await page.locator("#username").fill(USER);
await page.locator("#pass").fill(PASS);
await page.locator("#login button").click();
await page.waitForSelector("#screen", { state: "attached", timeout: 15000 });
await page.waitForFunction(async () => (await fetch("/api/status", { credentials: "same-origin" })).ok, undefined, { timeout: 15000 });
await page.waitForFunction(() => {
  const c = document.querySelector(".wc-surface");
  return c && c.width > 100 && document.getElementById("screen").videoWidth > 0;
}, undefined, { timeout: 30000 });
await page.waitForTimeout(1500);

const sway = (args) => execFileSync("swaymsg", args, {
  env: { ...process.env, XDG_RUNTIME_DIR: "/run/user/1000", WAYLAND_DISPLAY: "wayland-1", SWAYSOCK: execFileSync("bash", ["-lc", "ls /run/user/1000/sway-ipc.* | head -1"]).toString().trim() },
});

const results = [];
for (let i = 0; i < ROUNDS; i += 1) {
  const solid = i % 2 === 0;
  sway(["output", "HEADLESS-1", "bg", solid ? "#202020" : WALL, solid ? "solid_color" : "fill"]);
  const round = await page.evaluate(async () => {
    const video = document.getElementById("screen");
    const overlay = document.querySelector(".wc-surface");
    const W = 160, H = 90;
    const grab = (source) => {
      const c = document.createElement("canvas");
      c.width = W; c.height = H;
      const ctx = c.getContext("2d", { willReadFrequently: true });
      ctx.drawImage(source, 0, 0, W, H);
      return ctx.getImageData(0, 0, W, H).data;
    };
    const diff = (a, b) => {
      let n = 0;
      for (let k = 0; k < a.length; k += 4) if (Math.abs(a[k] - b[k]) > 18) n += 1;
      return n;
    };
    // baseline a moment after the change was issued but before it is visible
    const baseVideo = grab(video);
    const baseOverlay = grab(overlay);
    let videoAt = null, overlayAt = null;
    const start = performance.now();
    while (performance.now() - start < 2000 && (videoAt == null || overlayAt == null)) {
      await new Promise((r) => requestAnimationFrame(r));
      if (overlayAt == null && diff(grab(overlay), baseOverlay) > 40) overlayAt = performance.now();
      if (videoAt == null && diff(grab(video), baseVideo) > 40) videoAt = performance.now();
    }
    return { videoAt, overlayAt };
  });
  if (round.videoAt != null && round.overlayAt != null) results.push(round);
  await page.waitForTimeout(700);
}

const deltas = results.map((r) => r.videoAt - r.overlayAt).sort((a, b) => a - b);
console.log(`rounds=${results.length}`);
if (deltas.length) {
  const p50 = deltas[Math.floor(deltas.length / 2)];
  console.log(`element-visible minus canvas-visible: p50 ${p50.toFixed(1)} ms  (min ${deltas[0].toFixed(1)}, max ${deltas[deltas.length - 1].toFixed(1)})`);
  console.log(deltas.map((d) => d.toFixed(0)).join(" "));
  console.log(p50 > 2 ? "canvas leads" : p50 < -2 ? "element leads" : "no meaningful difference");
}
await browser.close();
