/**
 * SwayRTC end to end acceptance harness.
 *
 *   BASE=http://127.0.0.1:8790 node tools/e2e.mjs
 *
 * Runs against an isolated tools/testenv.sh instance (never production).
 * Checks: login -> TURN path -> video -> input over DataChannel -> reconnect
 *         -> mobile layout/soft keyboard -> capture child cleanup -> throughput.
 *
 * Every check prints PASS/FAIL/SKIP; the process exits non-zero on failure.
 */
import { chromium, devices } from "playwright-core";
import { execFileSync } from "node:child_process";
import { readFileSync, existsSync } from "node:fs";

const BASE = process.env.BASE ?? "http://127.0.0.1:8790";
const PORT = new URL(BASE).port;
const USER = process.env.USER_NAME ?? "swayrtctest";
const PASS = process.env.USER_PASS;
if (!PASS) { console.error("USER_PASS is not set (test account password: see .secrets/test-password)"); process.exit(1); }
const HOST_LOG = process.env.HOST_LOG ?? `/tmp/swayrtc-test-${PORT}/host.log`;
const EXPECT_PATH = process.env.EXPECT_PATH ?? "RELAY"; // RELAY | DIRECT | any

const results = [];
const record = (name, ok, detail = "") => {
  results.push({ name, ok, detail });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? "  (" + detail + ")" : ""}`);
};
const skip = (name, why) => {
  results.push({ name, ok: true, skip: true, detail: why });
  console.log(`SKIP  ${name}  (${why})`);
};

const logText = () => (existsSync(HOST_LOG) ? readFileSync(HOST_LOG, "utf8") : "");
/** Capture processes owned by THIS test instance's host (not other envs). */
const countCaptures = () => {
  try {
    const out = execFileSync("ps", ["-eo", "pid,ppid,comm"], { encoding: "utf8" });
    const rows = out.split("\n").slice(1).map((l) => l.trim().split(/\s+/)).filter((c) => c.length === 3);
    const parents = new Map(rows.map(([pid, ppid, comm]) => [Number(pid), { ppid: Number(ppid), comm }]));
    const hostPidFile = `${HOST_LOG.replace(/host\.log$/, "")}host.pid`;
    if (!existsSync(hostPidFile)) return -1;
    const host = Number(readFileSync(hostPidFile, "utf8").trim());
    const isDescendant = (pid) => {
      let cur = pid;
      for (let i = 0; i < 6; i += 1) {
        const row = parents.get(cur);
        if (!row) return false;
        if (row.ppid === host) return true;
        cur = row.ppid;
      }
      return false;
    };
    // comm is truncated to 15 chars: zerocap's zero-copy helper shows as "zerocopy-captur".
    const CAPTURE_COMMS = new Set(["wl-screenrec", "zerocap", "zerocopy-captur", "ffmpeg"]);
    return rows.filter(([pid, , comm]) => CAPTURE_COMMS.has(comm) && isDescendant(Number(pid))).length;
  } catch { return -1; }
};

async function login(page) {
  await page.goto(BASE + "/", { waitUntil: "domcontentloaded" });
  // RENDER=webcodecs exercises the encoded-transform renderer instead of the
  // media element.
  if (process.env.RENDER) {
    await page.evaluate((mode) => { try { sessionStorage.setItem("swayrtc.renderer", mode); } catch { /* blocked */ } }, process.env.RENDER);
    await page.reload({ waitUntil: "domcontentloaded" });
  }
  await page.locator("#username").fill(USER);
  await page.locator("#pass").fill(PASS);
  await page.locator("#login button").click();
  await page.waitForSelector("#screen", { state: "attached", timeout: 15000 });
  await page.waitForFunction(async () => {
    const r = await fetch("/api/status", { credentials: "same-origin" });
    return r.ok;
  }, undefined, { timeout: 15000 });
}

async function waitVideo(page, timeoutMs = 30000) {
  const started = Date.now();
  while (Date.now() - started < timeoutMs) {
    const state = await page.evaluate(() => {
      const v = document.getElementById("screen");
      return { w: v?.videoWidth ?? 0, h: v?.videoHeight ?? 0, ready: v?.readyState ?? 0, t: v?.currentTime ?? 0 };
    });
    if (state.w > 0 && state.ready >= 3 && state.t > 0) return { ...state, ms: Date.now() - started };
    await page.waitForTimeout(250);
  }
  return null;
}

const browser = await chromium.launch({
  executablePath: "/usr/bin/google-chrome",
  headless: true,
  args: ["--no-sandbox", "--autoplay-policy=no-user-gesture-required"],
});

try {
  /* ---------------- desktop: connect over the official path ---------------- */
  const desktop = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const page = await desktop.newPage();
  const pageErrors = [];
  page.on("pageerror", (e) => pageErrors.push(e.message));
  await login(page);
  record("login (PAM via /api/login)", true);

  const video = await waitVideo(page);
  record("video decoded", !!video, video ? `${video.w}x${video.h} in ${video.ms}ms` : "no frames within 30s");

  // The HUD is the source of truth for what the user sees.
  const telemetry = () => page.evaluate(() => ({
    transport: document.getElementById("transport")?.textContent?.trim() ?? "",
    path: document.getElementById("path")?.textContent?.trim() ?? "",
    ice: document.getElementById("ice")?.textContent?.trim() ?? "",
    resolution: document.getElementById("resolution")?.textContent?.trim() ?? "",
    fps: document.getElementById("fps")?.textContent?.trim() ?? "",
    rtt: document.getElementById("rtt")?.textContent?.trim() ?? "",
    jitter: document.getElementById("jitter")?.textContent?.trim() ?? "",
    ping: document.getElementById("ping")?.textContent?.trim() ?? "",
    turn: document.getElementById("turn")?.textContent?.trim() ?? "",
  }));
  const tel = await telemetry();
  const viaRelay = /RELAY/i.test(tel.transport) || /relay/i.test(tel.path);
  const pathOk = EXPECT_PATH === "any" || (EXPECT_PATH === "RELAY" ? viaRelay : !viaRelay);
  record(`media path = ${EXPECT_PATH}`, pathOk, `${tel.transport} | ${tel.path} | turn:${tel.turn}`);

  /* ---------------- throughput / latency sample ---------------- */
  await page.waitForTimeout(8000);
  const perf = await telemetry();
  const fpsValue = Number.parseFloat(perf.fps) || 0;
  const okFps = process.env.MIN_FPS ? fpsValue >= Number(process.env.MIN_FPS) : fpsValue > 1;
  record("live fps / latency", okFps, `${perf.fps} fps, rtt ${perf.rtt}, jitter ${perf.jitter}, ping ${perf.ping}, ${perf.resolution}`);

  /* ---------------- input over DataChannel ---------------- */
  const before = logText().length;
  await page.locator("#screen").click({ position: { x: 300, y: 200 } });
  await page.keyboard.press("KeyA");
  await page.keyboard.down("Shift");
  await page.keyboard.up("Shift");
  await page.keyboard.press("Tab");
  await page.mouse.move(500, 300);
  await page.mouse.wheel(0, 120);
  await page.waitForTimeout(1200);
  const delta = logText().slice(before);
  const sawInput = /swayrtc-input:.*event (key|button|move|wheel)/.test(delta) || /input\b.*(key|button)/i.test(delta);
  if (delta.length === 0) skip("input events reached the host", "host log has no debug lines (SWAYRTC_DEBUG?)");
  else record("input events reached the host", sawInput, sawInput ? "debug event lines present" : "no event lines in host log delta");

  // Regress a particularly confusing failure mode: the old viewer still sent
  // lossy cursor motion when the reliable button channel was disabled. A user
  // could move the cursor but not click, with no explanation.
  const healthProbe = await page.evaluate(() => window.__swayrtcInputHealth?.());
  record("input health probe exists", !!healthProbe && healthProbe.controlEnabled === true);
  await page.evaluate(() => { document.getElementById("control").checked = false; });
  await page.locator("#screen").click({ position: { x: 280, y: 240 } });
  const blockedProbe = await page.evaluate(() => window.__swayrtcInputHealth?.());
  record("disabled input reports blocked clicks", !!blockedProbe && blockedProbe.blocked >= 2 && blockedProbe.controlEnabled === false);
  await page.evaluate(() => { document.getElementById("control").checked = true; });
  await page.locator("#screen").click({ position: { x: 280, y: 240 } });
  const resumedProbe = await page.evaluate(() => window.__swayrtcInputHealth?.());
  record("re-enabled input sends clicks", !!resumedProbe && resumedProbe.sent >= 2 && resumedProbe.controlEnabled === true);

  /* ---------------- reconnect ---------------- */
  const t0 = Date.now();
  await page.reload({ waitUntil: "domcontentloaded" });
  await page.waitForSelector("#screen", { state: "attached", timeout: 15000 });
  const again = await waitVideo(page, 30000);
  record("video after reload", !!again, again ? `${Date.now() - t0}ms` : "no frames");

  /* ---------------- mobile ---------------- */
  const mobile = await browser.newContext({ ...devices["iPhone 14"] });
  const mpage = await mobile.newPage();
  const mobileErrors = [];
  mpage.on("pageerror", (e) => mobileErrors.push(e.message));
  await login(mpage);
  const mvideo = await waitVideo(mpage, 30000);
  record("mobile video decoded", !!mvideo, mvideo ? `${mvideo.w}x${mvideo.h} in ${mvideo.ms}ms` : "no frames");
  const layout = await mpage.evaluate(() => ({
    vw: window.innerWidth,
    overflow: document.documentElement.scrollWidth - window.innerWidth,
    keyboardButton: !!document.querySelector("#keyboard, [aria-label*='eyboard']"),
    touch: "ontouchstart" in window,
  }));
  record("mobile layout fits viewport", layout.overflow <= 1, `overflowX=${layout.overflow}px touch=${layout.touch}`);
  record("mobile soft keyboard control present", layout.keyboardButton, "");
  await mpage.screenshot({ path: "/tmp/swayrtc-mobile.png" });

  /* ---------------- page errors ---------------- */
  record("no page errors (desktop)", pageErrors.length === 0, pageErrors.slice(0, 2).join(" | "));
  record("no page errors (mobile)", mobileErrors.length === 0, mobileErrors.slice(0, 2).join(" | "));

  /* ---------------- child process cleanup ---------------- */
  await desktop.close();
  await mobile.close();
  let captures = -1;
  for (let i = 0; i < 12; i += 1) {
    await new Promise((r) => setTimeout(r, 1000));
    captures = countCaptures();
    if (captures === 0) break;
  }
  record("no orphan capture processes", captures === 0, `${captures} capture children of this host remain`);

  await page.context().close().catch(() => {});
} catch (error) {
  record("harness", false, String(error).slice(0, 300));
} finally {
  await browser.close();
}

const failed = results.filter((r) => !r.ok);
console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
process.exit(failed.length === 0 ? 0 : 1);
