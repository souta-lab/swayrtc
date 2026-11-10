import { chromium } from "playwright-core";
const BASE = process.env.BASE ?? "http://127.0.0.1:8786";
const browser = await chromium.launch({ executablePath: "/usr/bin/google-chrome", headless: true, args: ["--no-sandbox","--autoplay-policy=no-user-gesture-required"] });
const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
await page.goto(BASE + "/", { waitUntil: "domcontentloaded" });
await page.locator("#username").fill("swayrtctest");
const PASS = process.env.USER_PASS;
if (!PASS) { console.error("USER_PASS is not set (test account password: see .secrets/test-password)"); process.exit(1); }
await page.locator("#pass").fill(PASS);
await page.locator("#login button").click();
await page.waitForSelector("#screen", { state: "attached", timeout: 15000 });
await page.waitForFunction(async () => (await fetch("/api/status", { credentials: "same-origin" })).ok, undefined, { timeout: 15000 });
await page.waitForFunction(() => { const v = document.getElementById("screen"); return v && v.videoWidth > 0; }, undefined, { timeout: 30000 });
const out = await page.evaluate(async () => {
  const ice = await (await fetch("/api/ice", { credentials: "same-origin" })).json();
  const wsUrl = (location.protocol === "https:" ? "wss:" : "ws:") + "//" + location.host + "/ws/viewer";
  const pc = new RTCPeerConnection({ iceServers: ice.iceServers, iceTransportPolicy: "relay", bundlePolicy: "max-bundle" });
  pc.addTransceiver("video", { direction: "recvonly" });
  const dc = pc.createDataChannel("control", { ordered: false, maxRetransmits: 0 });
  const ws = new WebSocket(wsUrl);
  await new Promise((r, j) => { ws.onopen = r; ws.onerror = j; });
  ws.onmessage = async (ev) => {
    const m = JSON.parse(ev.data);
    if (m.type === "status" && m.host) { const o = await pc.createOffer(); await pc.setLocalDescription(o); ws.send(JSON.stringify({ type: "offer", sdp: pc.localDescription })); }
    else if (m.type === "answer") await pc.setRemoteDescription(m.sdp).catch(() => {});
    else if (m.type === "candidate" && pc.remoteDescription) await pc.addIceCandidate(m.candidate).catch(() => {});
  };
  pc.onicecandidate = (e) => { if (e.candidate) ws.send(JSON.stringify({ type: "candidate", candidate: e.candidate.toJSON() })); };
  await new Promise((r) => { pc.onconnectionstatechange = () => { if (pc.connectionState === "connected") r(); }; });
  while (dc.readyState !== "open") await new Promise((r) => setTimeout(r, 50));
  dc.send(JSON.stringify({ type: "move", relative: true, dx: 100, dy: 50 }));
  await new Promise((r) => setTimeout(r, 150));
  dc.send(JSON.stringify({ type: "key", code: "KeyB", key: "b", down: true, ctrlKey: true }));
  dc.send(JSON.stringify({ type: "key", code: "KeyB", key: "b", down: false }));
  await new Promise((r) => setTimeout(r, 150));
  dc.send(JSON.stringify({ type: "text", text: "こんにちは世界" }));
  await new Promise((r) => setTimeout(r, 1800));
  pc.close(); ws.close();
  return "sent";
});
console.log("SENT", out);
await browser.close();
