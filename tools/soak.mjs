// Long-running stability probe: keeps a session open and samples the HUD plus
// host memory every 30s. Usage: BASE=... DURATION=300 node tools/soak.mjs
import { chromium } from "playwright-core";
import { readFileSync } from "node:fs";

const BASE = process.env.BASE ?? "http://127.0.0.1:8780";
const DURATION = Number(process.env.DURATION ?? 300);
const HOST_PID = process.env.HOST_PID ? Number(process.env.HOST_PID) : null;

const rss = (pid) => {
  try {
    const status = readFileSync(`/proc/${pid}/status`, "utf8");
    return Number(/VmRSS:\s+(\d+)/.exec(status)?.[1] ?? 0);
  } catch { return 0; }
};

const browser = await chromium.launch({ executablePath: "/usr/bin/google-chrome", headless: true, args: ["--no-sandbox", "--autoplay-policy=no-user-gesture-required"] });
const page = await browser.newPage({ viewport: { width: 1920, height: 1080 } });
page.on("pageerror", (e) => console.log("PAGE_ERROR", e.message));
await page.goto(BASE + "/", { waitUntil: "domcontentloaded" });
await page.locator("#username").fill("swayrtctest");
const PASS = process.env.USER_PASS;
if (!PASS) { console.error("USER_PASS is not set (test account password: see .secrets/test-password)"); process.exit(1); }
await page.locator("#pass").fill(PASS);
await page.locator("#login button").click();
await page.waitForSelector("#screen", { state: "attached", timeout: 15000 });
await page.waitForFunction(async () => (await fetch("/api/status", { credentials: "same-origin" })).ok, undefined, { timeout: 15000 });
await page.waitForFunction(() => { const v = document.getElementById("screen"); return v && v.videoWidth > 0 && v.currentTime > 0; }, undefined, { timeout: 30000 });

const started = Date.now();
const samples = [];
while (Date.now() - started < DURATION * 1000) {
  await page.waitForTimeout(30000);
  const t = await page.evaluate(() => ({
    fps: document.getElementById("fps")?.textContent,
    rtt: document.getElementById("rtt")?.textContent,
    jitter: document.getElementById("jitter")?.textContent,
    loss: document.getElementById("loss")?.textContent,
    transport: document.getElementById("transport")?.textContent,
    video: (() => { const v = document.getElementById("screen"); return { w: v.videoWidth, h: v.videoHeight, t: v.currentTime, ready: v.readyState }; })(),
  }));
  const row = { at: Math.round((Date.now() - started) / 1000), ...t, rssKB: HOST_PID ? rss(HOST_PID) : 0 };
  samples.push(row);
  console.log(JSON.stringify(row));
  if (row.video.ready < 3 || row.video.w === 0) { console.log("STALL_DETECTED"); break; }
}
const stalled = samples.filter((s) => s.video.ready < 3 || s.video.w === 0).length;
const rssValues = samples.map((s) => s.rssKB).filter((x) => x > 0);
console.log("SUMMARY", JSON.stringify({
  samples: samples.length,
  stalled,
  rssFirstKB: rssValues[0] ?? 0,
  rssLastKB: rssValues.at(-1) ?? 0,
  rssGrowthKB: (rssValues.at(-1) ?? 0) - (rssValues[0] ?? 0),
  playheadAdvanced: (samples.at(-1)?.video.t ?? 0) > (samples[0]?.video.t ?? 0),
}));
await browser.close();
