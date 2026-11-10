import { chromium } from "playwright-core";

const URL_BASE = process.env.BASE || "http://127.0.0.1:8778";
const USER = process.env.USER_NAME || "swayrtctest";
const PASS = process.env.USER_PASS;
if (!PASS) { console.error("USER_PASS is not set (test account password: see .secrets/test-password)"); process.exit(1); }

const browser = await chromium.launch({
  executablePath: "/usr/bin/google-chrome",
  headless: true,
  args: ["--no-sandbox", "--autoplay-policy=no-user-gesture-required"],
});
const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
page.on("pageerror", (e) => console.log("PAGE_ERROR", e.message));
page.on("console", (m) => console.log("CONSOLE", m.type(), m.text().slice(0, 300)));
page.on("websocket", (ws) => {
  console.log("WS_OPEN", ws.url());
  ws.on("framesent", (f) => {
    const p = String(f.payload);
    console.log("WS_SEND", p.length > 200 ? p.slice(0, 200) + "..." : p);
  });
  ws.on("framereceived", (f) => {
    const p = String(f.payload);
    console.log("WS_RECV", p.length > 200 ? p.slice(0, 200) + "..." : p);
  });
  ws.on("close", () => console.log("WS_CLOSE"));
});

try {
  await page.goto(URL_BASE + "/", { waitUntil: "domcontentloaded" });
  console.log("TITLE", await page.title());
  await page.locator("#username").fill(USER);
  await page.locator("#pass").fill(PASS);
  await page.locator("#login button").click();
  await page.waitForURL(URL_BASE + "/", { timeout: 15000 });
  console.log("AUTHENTICATED");
  for (let i = 0; i < 12; i++) {
    await page.waitForTimeout(2000);
    const s = await page.evaluate(() => ({
      status: document.getElementById("status")?.textContent,
      ice: document.getElementById("ice")?.textContent,
      transport: document.getElementById("transport")?.textContent,
      headline: document.getElementById("headline")?.textContent,
      detail: document.getElementById("detail")?.textContent,
      video: (() => { const v = document.getElementById("screen"); return { w: v.videoWidth, h: v.videoHeight, ready: v.readyState, paused: v.paused }; })(),
    }));
    console.log("T+" + (i + 1) * 2 + "s", JSON.stringify(s));
    if (s.video.w > 0 && s.video.ready >= 3) { console.log("VIDEO_OK"); break; }
  }
} catch (e) {
  console.log("TEST_FAILURE", e.message.slice(0, 600));
}
await browser.close();
