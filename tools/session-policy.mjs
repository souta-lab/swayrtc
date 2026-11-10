// Verifies the session policy: newest viewer wins, but a viewer that is idle for
// 30 minutes loses the session (and the host stops capturing).
import { chromium } from "playwright-core";

const BASE = process.env.BASE ?? "http://127.0.0.1:8787";
const USER = process.env.USER_NAME ?? "swayrtctest";
const PASS = process.env.USER_PASS;
if (!PASS) { console.error("USER_PASS is not set (test account password: see .secrets/test-password)"); process.exit(1); }
const results = [];
const check = (name, ok, detail = "") => { results.push({ name, ok }); console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? "  (" + detail + ")" : ""}`); };

const browser = await chromium.launch({ executablePath: "/usr/bin/google-chrome", headless: true, args: ["--no-sandbox", "--autoplay-policy=no-user-gesture-required"] });

async function openViewer(context) {
  const page = await context.newPage();
  await page.goto(BASE + "/", { waitUntil: "domcontentloaded" });
  await page.locator("#username").fill(USER);
  await page.locator("#pass").fill(PASS);
  await page.locator("#login button").click();
  await page.waitForSelector("#screen", { state: "attached", timeout: 15000 });
  await page.waitForFunction(async () => (await fetch("/api/status", { credentials: "same-origin" })).ok, undefined, { timeout: 15000 });
  return page;
}

const ctxA = await browser.newContext({ viewport: { width: 1280, height: 800 } });
const ctxB = await browser.newContext({ viewport: { width: 1280, height: 800 } });

// --- newest viewer wins -------------------------------------------------
const a = await openViewer(ctxA);
await a.waitForFunction(() => { const v = document.getElementById("screen"); return v && v.videoWidth > 0; }, undefined, { timeout: 30000 });
check("viewer A connected", true);
const b = await openViewer(ctxB);
await a.waitForFunction(() => /Session|moved|replaced/i.test(document.body.innerText), undefined, { timeout: 15000 }).catch(() => {});
const aText = await a.evaluate(() => document.body.innerText);
check("viewer A told the session moved", /Session moved|another tab|replaced|Session moved to/i.test(aText), aText.split("\n").filter(Boolean).slice(0, 2).join(" / "));
// A must not steal the session back
await new Promise((r) => setTimeout(r, 6000));
const bState = await b.evaluate(() => ({ status: document.getElementById("status")?.textContent, w: document.getElementById("screen")?.videoWidth }));
const aStillOut = await a.evaluate(() => /Session moved|another tab|replaced/i.test(document.body.innerText));
check("viewer B keeps the session", bState.w > 0, `B: ${bState.status} ${bState.w}px`);
check("viewer A did not reconnect over B", aStillOut);
await b.waitForFunction(() => { const v = document.getElementById("screen"); return v && v.videoWidth > 0; }, undefined, { timeout: 30000 });
check("viewer B has video", true);

// --- 30 minute idle timeout --------------------------------------------
await b.clock.install({ time: new Date() });
// re-arm the idle timer with the faked clock by reconnecting this viewer
await b.evaluate(() => window.dispatchEvent(new Event("online")));
await b.reload({ waitUntil: "domcontentloaded" });
await b.waitForSelector("#screen", { state: "attached", timeout: 15000 });
await b.waitForFunction(async () => (await fetch("/api/status", { credentials: "same-origin" })).ok, undefined, { timeout: 15000 });
await b.waitForFunction(() => { const v = document.getElementById("screen"); return v && v.videoWidth > 0; }, undefined, { timeout: 30000 });
await b.clock.runFor(20 * 60 * 1000);
const midText = await b.evaluate(() => document.body.innerText);
check("still connected after 20 min idle", !/closed after/i.test(midText));
await b.clock.runFor(11 * 60 * 1000);
const endText = await b.evaluate(() => document.body.innerText);
check("session closed after 30 min idle", /closed after/i.test(endText), endText.replace(/\n+/g, " ").slice(0, 90));

console.log(`\n${results.filter((r) => r.ok).length}/${results.length} checks passed`);
await browser.close();
process.exit(results.every((r) => r.ok) ? 0 : 1);
