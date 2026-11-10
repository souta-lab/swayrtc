// Verifies that key events survive the transport: no drops, correct order,
// down/up pairing. A lost keyup leaves the key held and the compositor repeats
// it, which is what "one tap produced a burst" looks like.
import { chromium } from "playwright-core";
import { readFileSync, existsSync } from "node:fs";

const BASE = process.env.BASE ?? "http://127.0.0.1:8805";
const HOST_LOG = process.env.HOST_LOG ?? `/tmp/swayrtc-test-${new URL(BASE).port}/host.log`;
const USER = "swayrtctest", PASS = process.env.USER_PASS;
if (!PASS) { console.error("USER_PASS is not set (test account password: see .secrets/test-password)"); process.exit(1); }
const N = Number(process.env.N ?? 200);

const browser = await chromium.launch({ executablePath: "/usr/bin/google-chrome", headless: true, args: ["--no-sandbox", "--autoplay-policy=no-user-gesture-required"] });
const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
await page.goto(BASE + "/", { waitUntil: "domcontentloaded" });
await page.locator("#username").fill(USER);
await page.locator("#pass").fill(PASS);
await page.locator("#login button").click();
await page.waitForSelector("#screen", { state: "attached", timeout: 15000 });
await page.waitForFunction(async () => (await fetch("/api/status", { credentials: "same-origin" })).ok, undefined, { timeout: 15000 });
await page.waitForFunction(() => { const v = document.getElementById("screen"); return v && v.videoWidth > 0; }, undefined, { timeout: 30000 });
// Count what the page actually puts on the wire, per channel.
await page.evaluate(() => {
  window.__sent = { key: 0, control: 0, motion: 0 };
  const proto = RTCDataChannel.prototype;
  const orig = proto.send;
  proto.send = function (payload) {
    try {
      const m = JSON.parse(payload);
      if (this.label === "motion") window.__sent.motion += 1;
      else { window.__sent.control += 1; if (m.type === "key") window.__sent.key += 1; }
    } catch { /* binary */ }
    return orig.call(this, payload);
  };
});
const before = existsSync(HOST_LOG) ? readFileSync(HOST_LOG, "utf8").length : 0;

await page.evaluate(async (n) => {
  const screen = document.getElementById("screen");
  for (let i = 0; i < n; i += 1) {
    const code = "KeyA";
    screen.dispatchEvent(new KeyboardEvent("keydown", { code, key: "a", bubbles: true }));
    screen.dispatchEvent(new KeyboardEvent("keyup", { code, key: "a", bubbles: true }));
    if (i % 10 === 0) await new Promise((r) => setTimeout(r, 5));
  }
}, N);
await page.waitForTimeout(2500);
const sentCounts = await page.evaluate(() => window.__sent);
console.log("page sent:", JSON.stringify(sentCounts));
await browser.close();

const log = (existsSync(HOST_LOG) ? readFileSync(HOST_LOG, "utf8") : "").slice(before);
const events = [...log.matchAll(/event key code=Some\((\d+)\) key="([^"]*)" down=(true|false)/g)].map((m) => ({ down: m[3] === "true" }));
const downs = events.filter((e) => e.down).length;
const ups = events.length - downs;
// pairing: walk the sequence, every up must follow an outstanding down
let outstanding = 0, broken = 0;
for (const e of events) {
  if (e.down) outstanding += 1;
  else { if (outstanding > 0) outstanding -= 1; else broken += 1; }
}
console.log(`sent ${N} down/up pairs -> host saw ${downs} downs, ${ups} ups, stranded=${outstanding}, unmatched ups=${broken}`);
const ok = downs === N && ups === N && outstanding === 0;
console.log(ok ? "PASS: every key event arrived and paired" : "FAIL: key events lost or reordered");
process.exit(ok ? 0 : 1);
