// End to end latency: send a pointer move, then measure how long it takes for
// the moved cursor to appear in the received video. This is exactly the delay a
// user feels when they move the mouse.
//
// Both ends run on this machine, so Date.now() is a shared clock: the sender
// stamps the message and the receiver observes the frame. The measurement
// therefore covers input path + compositor + capture + encode + network + jitter
// buffer + decode, i.e. the whole loop.
//
// Usage: BASE=http://127.0.0.1:8785 node tools/e2e-latency.mjs
import { chromium } from "playwright-core";

const BASE = process.env.BASE ?? "http://127.0.0.1:8785";
const USER = process.env.USER_NAME ?? "swayrtctest";
const PASS = process.env.USER_PASS;
if (!PASS) { console.error("USER_PASS is not set (test account password: see .secrets/test-password)"); process.exit(1); }
const ROUNDS = Number(process.env.ROUNDS ?? 6);

const browser = await chromium.launch({
  executablePath: "/usr/bin/google-chrome",
  headless: true,
  args: ["--no-sandbox", "--autoplay-policy=no-user-gesture-required"],
});
const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
await page.goto(BASE + "/", { waitUntil: "domcontentloaded" });
await page.locator("#username").fill(USER);
await page.locator("#pass").fill(PASS);
await page.locator("#login button").click();
await page.waitForSelector("#screen", { state: "attached", timeout: 15000 });
await page.waitForFunction(async () => (await fetch("/api/status", { credentials: "same-origin" })).ok, undefined, { timeout: 15000 });

const result = await page.evaluate(async (rounds) => {
  const ice = await (await fetch("/api/ice", { credentials: "same-origin" })).json();
  const wsUrl = (location.protocol === "https:" ? "wss:" : "ws:") + "//" + location.host + "/ws/viewer";
  const pc = new RTCPeerConnection({ iceServers: ice.iceServers, iceTransportPolicy: "relay", bundlePolicy: "max-bundle" });
  pc.addTransceiver("video", { direction: "recvonly" });
  const dc = pc.createDataChannel("control", { ordered: false, maxRetransmits: 0 });
  const video = document.createElement("video");
  video.muted = true;
  video.autoplay = true;
  video.playsInline = true;
  pc.ontrack = (event) => { video.srcObject = event.streams[0] || new MediaStream([event.track]); };
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
  while (dc.readyState !== "open") await new Promise((r) => setTimeout(r, 50));
  await video.play().catch(() => {});
  while (!(video.videoWidth > 0 && video.readyState >= 3 && video.currentTime > 0)) await new Promise((r) => setTimeout(r, 100));

  const W = 240, H = 135;
  const canvas = document.createElement("canvas");
  canvas.width = W; canvas.height = H;
  const ctx = canvas.getContext("2d", { willReadFrequently: true });
  const grab = () => {
    // SURFACE lets the probe watch whatever is actually on screen: the media
    // element, or the WebCodecs canvas painted over it.
    const surface = (typeof process !== "undefined" && process.env && process.env.SURFACE) ? null : null;
    ctx.drawImage(globalThis.__probeSurface || video, 0, 0, W, H);
    return ctx.getImageData(0, 0, W, H).data;
  };
  // Diff restricted to a box around the target so unrelated screen updates
  // (clock, blinking caret) cannot be mistaken for the cursor arriving.
  const diffBox = (a, b, cx, cy, half) => {
    let peak = 0, changed = 0;
    for (let y = Math.max(0, cy - half); y < Math.min(H, cy + half); y += 1) {
      for (let x = Math.max(0, cx - half); x < Math.min(W, cx + half); x += 1) {
        const i = (y * W + x) * 4;
        const d = Math.abs(a[i] - b[i]) + Math.abs(a[i + 1] - b[i + 1]) + Math.abs(a[i + 2] - b[i + 2]);
        if (d > 24) changed += 1;
        if (d > peak) peak = d;
      }
    }
    return { peak, changed };
  };
  const toCanvas = (x, y) => [Math.round((x * W) / video.videoWidth), Math.round((y * H) / video.videoHeight)];
  const moveTo = (x, y, t) => dc.send(JSON.stringify({ type: "move", x, y, t: t ?? Date.now() }));

  // Establish a baseline with the cursor parked at a neutral spot.
  moveTo(960, 540);
  await new Promise((r) => setTimeout(r, 800));
  const samples = [];
  const spots = [[240, 200], [1680, 900], [400, 880], [1500, 160], [800, 300], [1200, 760]].slice(0, rounds);
  for (const [x, y] of spots) {
    const baseline = grab();
    await new Promise((r) => setTimeout(r, 120));
    const t0 = performance.now();
    const wall0 = Date.now();
    moveTo(x, y, wall0);
    // Poll frames until the picture changes (cursor arrived).
    let hit = null;
    const deadline = performance.now() + 1500;
    while (performance.now() < deadline) {
      await new Promise((r) => requestAnimationFrame(r));
      const now = grab();
      const [cx, cy] = toCanvas(x, y);
      const d = diffBox(baseline, now, cx, cy, 12);
      if (d.changed >= 3 && d.peak > 40) { hit = performance.now() - t0; break; }
    }
    samples.push({ spot: [x, y], ms: hit == null ? null : Math.round(hit), wall0, wallDetect: Date.now() });
    await new Promise((r) => setTimeout(r, 400));
  }
  moveTo(960, 540);
  pc.close();
  ws.close();
  return samples;
}, ROUNDS);

// Correlate with the host log: split the loop into the host leg (input arrival ->
// frame sent) and the browser leg (frame sent -> pixel changed on screen).
import { readFileSync, existsSync } from "node:fs";
const HOST_LOG = process.env.HOST_LOG ?? `/tmp/swayrtc-test-${new URL(BASE).port}/host.log`;
const logText = existsSync(HOST_LOG) ? readFileSync(HOST_LOG, "utf8") : "";
const recvTs = [...logText.matchAll(/event=input\.recv kind=move age_ms=(-?\d+) ts=(\d+)/g)].map((m) => Number(m[2]));
const frameTs = [...logText.matchAll(/event=frame\.tx gen=\d+ bytes=\d+ idr=\w+ read_age_ms=\d+ send_us=\d+ ts=(\d+)/g)].map((m) => Number(m[1]));
const legs = [];
for (const s of result) {
  if (s.ms == null || !s.wallDetect) continue;
  const recv = recvTs.find((t) => t >= s.wall0);
  if (recv == null) continue;
  const sent = frameTs.find((t) => t >= recv);
  if (sent == null) continue;
  legs.push({ total: s.wallDetect - s.wall0, host: sent - recv, browser: s.wallDetect - sent, input: recv - s.wall0 });
}
if (legs.length) {
  const med = (k) => { const v = legs.map((l) => l[k]).sort((a, b) => a - b); return v[Math.floor(v.length / 2)]; };
  console.log(`legs (p50): input ${med("input")}ms + host(input arrival -> frame sent) ${med("host")}ms + browser(frame sent -> pixel) ${med("browser")}ms = ${med("total")}ms  [n=${legs.length}]`);
}

const values = result.map((r) => r.ms).filter((v) => typeof v === "number").sort((a, b) => a - b);
console.log("samples", JSON.stringify(result.map((r) => r.ms)));
if (values.length) {
  const p = (q) => values[Math.min(values.length - 1, Math.floor(q * values.length))];
  console.log(`end-to-end cursor latency ms: min=${values[0]} p50=${p(0.5)} max=${values.at(-1)} avg=${(values.reduce((a, b) => a + b, 0) / values.length).toFixed(0)}`);
} else {
  console.log("no hits: the cursor change was not detected in the video");
}
await browser.close();
