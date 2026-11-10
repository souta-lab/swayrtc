// Capture what the viewer actually renders, for visual comparison with grim.
import { chromium } from "playwright-core";

const BASE = process.env.BASE ?? "http://127.0.0.1:8790";
const OUT = process.env.OUT ?? "/tmp/viewer.png";
const USER = process.env.USER_NAME ?? "swayrtctest";
const PASS = process.env.USER_PASS;
if (!PASS) { console.error("USER_PASS is not set (test account password: see .secrets/test-password)"); process.exit(1); }

const browser = await chromium.launch({
  executablePath: "/usr/bin/google-chrome",
  headless: true,
  args: ["--no-sandbox", "--autoplay-policy=no-user-gesture-required"],
});
const page = await browser.newPage({ viewport: { width: 1600, height: 1000 } });
await page.goto(BASE + "/", { waitUntil: "domcontentloaded" });
await page.locator("#username").fill(USER);
await page.locator("#pass").fill(PASS);
await page.locator("#login button").click();
await page.waitForSelector("#screen", { state: "attached", timeout: 15000 });
await page.waitForFunction(async () => (await fetch("/api/status", { credentials: "same-origin" })).ok, undefined, { timeout: 15000 });

const start = Date.now();
let state = null;
while (Date.now() - start < 30000) {
  state = await page.evaluate(() => {
    const v = document.getElementById("screen");
    return { w: v?.videoWidth ?? 0, h: v?.videoHeight ?? 0, r: v?.readyState ?? 0, t: v?.currentTime ?? 0 };
  });
  if (state.w > 0 && state.r >= 3 && state.t > 0) break;
  await page.waitForTimeout(200);
}
console.log("video", JSON.stringify(state), "after", Date.now() - start, "ms");
await page.waitForTimeout(3000);

// Full page screenshot plus a screenshot of the raw video frame (no UI scaling).
await page.screenshot({ path: OUT });
await page.evaluate(() => {
  const v = document.getElementById("screen");
  const c = document.createElement("canvas");
  c.id = "__grab";
  c.width = v.videoWidth; c.height = v.videoHeight;
  c.style.cssText = "position:fixed;inset:0;z-index:99999;width:100%;height:100%";
  c.getContext("2d").drawImage(v, 0, 0);
  document.body.appendChild(c);
});
const grab = await page.locator("#__grab").screenshot({ path: OUT.replace(/\.png$/, "-frame.png") });
console.log("saved", OUT, "and", OUT.replace(/\.png$/, "-frame.png"), grab.length, "bytes");
await browser.close();
