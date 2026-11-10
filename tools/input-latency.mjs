// Measures one way input latency: viewer DataChannel send -> host receive.
// Both processes share this machine's clock, so the host can report (now - t).
import { chromium } from "playwright-core";
import { readFileSync, existsSync } from "node:fs";

const BASE = process.env.BASE ?? "http://127.0.0.1:8785";
const HOST_LOG = process.env.HOST_LOG ?? `/tmp/swayrtc-test-${new URL(BASE).port}/host.log`;
const USER = process.env.USER_NAME ?? "swayrtctest";
const PASS = process.env.USER_PASS;
if (!PASS) { console.error("USER_PASS is not set (test account password: see .secrets/test-password)"); process.exit(1); }

const browser = await chromium.launch({ executablePath: "/usr/bin/google-chrome", headless: true, args: ["--no-sandbox", "--autoplay-policy=no-user-gesture-required"] });
const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
await page.goto(BASE + "/", { waitUntil: "domcontentloaded" });
await page.locator("#username").fill(USER);
await page.locator("#pass").fill(PASS);
await page.locator("#login button").click();
await page.waitForSelector("#screen", { state: "attached", timeout: 15000 });
await page.waitForFunction(async () => (await fetch("/api/status", { credentials: "same-origin" })).ok, undefined, { timeout: 15000 });

const sent = await page.evaluate(async () => {
  const ice = await (await fetch("/api/ice", { credentials: "same-origin" })).json();
  const wsUrl = (location.protocol === "https:" ? "wss:" : "ws:") + "//" + location.host + "/ws/viewer";
  const pc = new RTCPeerConnection({ iceServers: ice.iceServers, iceTransportPolicy: "relay", bundlePolicy: "max-bundle" });
  pc.addTransceiver("video", { direction: "recvonly" });
  const dc = pc.createDataChannel("control", { ordered: false, maxRetransmits: 0 });
  const ws = new WebSocket(wsUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  ws.onmessage = async (ev) => {
    const m = JSON.parse(ev.data);
    if (m.type === "status" && m.host) {
      const offer = await pc.createOffer();
      await pc.setLocalDescription(offer);
      ws.send(JSON.stringify({ type: "offer", sdp: pc.localDescription }));
    } else if (m.type === "answer") await pc.setRemoteDescription(m.sdp).catch(() => {});
    else if (m.type === "candidate" && pc.remoteDescription) await pc.addIceCandidate(m.candidate).catch(() => {});
  };
  pc.onicecandidate = (e) => { if (e.candidate) ws.send(JSON.stringify({ type: "candidate", candidate: e.candidate.toJSON() })); };
  await new Promise((resolve) => { pc.onconnectionstatechange = () => { if (pc.connectionState === "connected") resolve(); }; });
  await new Promise((r) => setTimeout(r, 2500));
  const stamps = [];
  for (let i = 0; i < 30; i += 1) {
    const t = Date.now();
    stamps.push(t);
    dc.send(JSON.stringify({ type: "move", x: 200 + i * 3, y: 300 + i, t }));
    await new Promise((r) => setTimeout(r, 50));
  }
  await new Promise((r) => setTimeout(r, 800));
  pc.close();
  ws.close();
  return stamps.length;
});

await new Promise((r) => setTimeout(r, 500));
const log = existsSync(HOST_LOG) ? readFileSync(HOST_LOG, "utf8") : "";
const ages = [...log.matchAll(/event=input\.recv kind=move age_ms=(-?\d+)/g)].map((m) => Number(m[1]));
const recent = ages.slice(-sent);
const sorted = [...recent].sort((a, b) => a - b);
const pick = (q) => sorted[Math.min(sorted.length - 1, Math.floor(q * sorted.length))];
console.log(`sent=${sent} received=${recent.length}`);
if (recent.length) {
  console.log(`one-way input latency ms: min=${sorted[0]} p50=${pick(0.5)} p90=${pick(0.9)} max=${sorted.at(-1)} avg=${(recent.reduce((a, b) => a + b, 0) / recent.length).toFixed(1)}`);
} else {
  console.log("no samples (is SWAYRTC_DEBUG=1 set for the host?)");
}
await browser.close();
