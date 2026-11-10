// Frame level latency breakdown without relying on cross process clock matching.
//
// Legs:
//   input transport   browser Date.now() -> host receive (the host logs the age)
//   compositor+encode pointer move received -> first encoded frame leaves
//   network           measured as DataChannel RTT / 2 (independent clocks)
//   buffering+present rVFC expectedDisplayTime - receiveTime
//   decode            rVFC processingDuration
//
// The stimulus is a pointer move: the host records when the input arrived and
// when the next access unit was sent, so the capture/encode leg is measured on a
// single clock. Repeating it with random phases makes the minimum approach the
// true pipeline latency (the compositor adds a random 0..16.7 ms slice).
//
// Usage: BASE=http://127.0.0.1:8788 node tools/latency-breakdown.mjs
import { chromium } from "playwright-core";
import { readFileSync, existsSync } from "node:fs";

const BASE = process.env.BASE ?? "http://127.0.0.1:8788";
const HOST_LOG = process.env.HOST_LOG ?? `/tmp/swayrtc-test-${new URL(BASE).port}/host.log`;
const USER = process.env.USER_NAME ?? "swayrtctest";
const PASS = process.env.USER_PASS;
if (!PASS) { console.error("USER_PASS is not set (test account password: see .secrets/test-password)"); process.exit(1); }
const TRIALS = Number(process.env.TRIALS ?? 20);

const browser = await chromium.launch({ executablePath: "/usr/bin/google-chrome", headless: true, args: ["--no-sandbox", "--autoplay-policy=no-user-gesture-required"] });
const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
await page.goto(BASE + "/", { waitUntil: "domcontentloaded" });
await page.locator("#username").fill(USER);
await page.locator("#pass").fill(PASS);
await page.locator("#login button").click();
await page.waitForSelector("#screen", { state: "attached", timeout: 15000 });
await page.waitForFunction(async () => (await fetch("/api/status", { credentials: "same-origin" })).ok, undefined, { timeout: 15000 });
await page.waitForFunction(() => { const v = document.getElementById("screen"); return v && v.videoWidth > 0 && v.currentTime > 0; }, undefined, { timeout: 30000 });
await page.evaluate(() => window.__swayrtcPerf && window.__swayrtcPerf.reset());
await page.waitForTimeout(1500);

// --- stimulus: move the pointer so the host can stamp the follow-up frame ---
const sent = [];
for (let i = 0; i < TRIALS; i += 1) {
  const t = await page.evaluate(() => Date.now());
  await page.evaluate(({ i, t }) => {
    const ev = new PointerEvent("pointermove", {
      clientX: 100 + ((i * 137) % 800),
      clientY: 100 + ((i * 89) % 600),
      bubbles: true,
      pointerType: "mouse",
    });
    document.getElementById("screen").dispatchEvent(ev);
    window.__swayrtcStimulus = t;
  }, { i, t });
  sent.push(t);
  await page.waitForTimeout(120 + (i % 5) * 17);
}

const probe = await page.evaluate(() => ({
  summary: window.__swayrtcPerf ? window.__swayrtcPerf.summary() : null,
  channelRtt: document.getElementById("ping")?.textContent ?? null,
}));
await browser.close();

const log = existsSync(HOST_LOG) ? readFileSync(HOST_LOG, "utf8") : "";
const recvs = [...log.matchAll(/event=input\.recv kind=move age_ms=(-?\d+)/g)].map((m) => Number(m[1]));
const frames = [...log.matchAll(/event=frame\.tx gen=\d+ bytes=(\d+) idr=(\w+) read_age_ms=(\d+) send_us=(\d+) ts=(\d+)/g)]
  .map((m) => ({ bytes: Number(m[1]), readAgeMs: Number(m[3]), sendUs: Number(m[4]), ts: Number(m[5]) }));

if (!frames.length || !recvs.length) {
  console.log("not enough samples", { frames: frames.length, inputs: recvs.length });
  process.exit(1);
}

const arrivals = sent.map((t, i) => t + (recvs[i] ?? recvs.at(-1)));
const deltas = [];
for (const at of arrivals) {
  const next = frames.find((f) => f.ts >= at);
  if (next) deltas.push(next.ts - at);
}

const pct = (v, q) => {
  const s = [...v].filter((x) => Number.isFinite(x)).sort((a, b) => a - b);
  return s.length ? s[Math.min(s.length - 1, Math.floor(s.length * q))] : null;
};
const inputP50 = pct(recvs, 0.5);
const inputP90 = pct(recvs, 0.9);
const pipelineMin = deltas.length ? Math.min(...deltas) : null;
const pipelineP50 = pct(deltas, 0.5);
const rttMatch = /(\d+(?:\.\d+)?)/.exec(probe.channelRtt ?? "");
const channelRtt = rttMatch ? Number(rttMatch[1]) : null;
const networkOneWay = channelRtt != null ? channelRtt / 2 : null;
const summary = probe.summary ?? {};
const buffering = summary.displayDelayMs ? summary.displayDelayMs.p50 : null;
const decode = summary.processingMs ? summary.processingMs.p50 : null;
const frameGap = frames.length > 2 ? (frames.at(-1).ts - frames[0].ts) / (frames.length - 1) : null;

const pad = (label, value, unit = "ms") => console.log(`${label.padEnd(36)} ${value == null ? "—" : `${value.toFixed(1)} ${unit}`}`);
console.log(`samples: ${deltas.length} stimulus trials, ${frames.length} frames, ${recvs.length} input arrivals\n`);
pad("input transport (viewer -> host)", inputP50);
pad("  input p90", inputP90);
pad("compositor+capture+encode (min)", pipelineMin);
pad("  same, p50 (incl. frame phase)", pipelineP50);
pad("network one way (channel RTT / 2)", networkOneWay);
pad("buffering + presentation", buffering);
pad("H.264 decode", decode);
pad("frame gap at source", frameGap);
const total = [inputP50, pipelineMin, networkOneWay, buffering, decode].reduce((a, b) => (a == null || b == null ? null : a + b), 0);
console.log(`\n${"~ end to end (input -> pixels)".padEnd(36)} ${total == null ? "—" : `${total.toFixed(1)} ms`}`);
console.log(`channel RTT reported by the viewer: ${probe.channelRtt ?? "—"}`);
