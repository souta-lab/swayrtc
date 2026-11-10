// Measures where the latency actually goes.
//   RTT/2 (network, one way) + jitter buffer + decode = pipeline latency
// Usage: BASE=http://127.0.0.1:8782 node tools/latency.mjs
import { chromium } from "playwright-core";

const BASE = process.env.BASE ?? "http://127.0.0.1:8782";
const POLICY = process.env.POLICY ?? "relay";
const USER = process.env.USER_NAME ?? "swayrtctest";
const PASS = process.env.USER_PASS;
if (!PASS) { console.error("USER_PASS is not set (test account password: see .secrets/test-password)"); process.exit(1); }

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

await page.evaluate((noHints) => { window.__noHints = noHints; }, process.env.NOHINT === "1");
const result = await page.evaluate(async (policy) => {
  const ice = await (await fetch("/api/ice", { credentials: "same-origin" })).json();
  const wsUrl = (location.protocol === "https:" ? "wss:" : "ws:") + "//" + location.host + "/ws/viewer";
  const pc = new RTCPeerConnection({ iceServers: ice.iceServers, iceTransportPolicy: policy, bundlePolicy: "max-bundle" });
  pc.addTransceiver("video", { direction: "recvonly" });
  const channel = pc.createDataChannel("control");
  const hints = { playoutDelayHint: false, jitterBufferTarget: false };
  pc.ontrack = (event) => {
    const receiver = event.receiver;
    if (!window.__noHints) {
      try { receiver.playoutDelayHint = 0; hints.playoutDelayHint = receiver.playoutDelayHint === 0; } catch { }
      try { receiver.jitterBufferTarget = 0; hints.jitterBufferTarget = receiver.jitterBufferTarget === 0; } catch { }
    }
  };
  const ws = new WebSocket(wsUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  await new Promise((resolve) => {
    const timer = setTimeout(resolve, 25000);
    ws.onmessage = async (ev) => {
      const msg = JSON.parse(ev.data);
      if (msg.type === "status" && msg.host) {
        const offer = await pc.createOffer();
        await pc.setLocalDescription(offer);
        ws.send(JSON.stringify({ type: "offer", sdp: pc.localDescription }));
      } else if (msg.type === "answer") {
        await pc.setRemoteDescription(msg.sdp).catch(() => {});
      } else if (msg.type === "candidate" && pc.remoteDescription) {
        await pc.addIceCandidate(msg.candidate).catch(() => {});
      }
    };
    pc.onicecandidate = (e) => { if (e.candidate) ws.send(JSON.stringify({ type: "candidate", candidate: e.candidate.toJSON() })); };
    pc.onconnectionstatechange = () => { if (pc.connectionState === "connected") { clearTimeout(timer); setTimeout(resolve, 6000); } };
  });

  const stats = await pc.getStats();
  const out = { hints, frames: 0 };
  let pair = null;
  stats.forEach((s) => {
    if (s.type === "candidate-pair" && s.nominated && s.state === "succeeded") pair = s;
    if (s.type === "inbound-rtp" && s.kind === "video") {
      out.frames = s.framesDecoded;
      out.jitterBufferDelay = s.jitterBufferDelay;
      out.jitterBufferEmitted = s.jitterBufferEmittedCount;
      out.totalDecodeTime = s.totalDecodeTime;
      out.decodeAvgMs = s.framesDecoded ? (s.totalDecodeTime / s.framesDecoded) * 1000 : null;
      out.jitterBufferAvgMs = s.jitterBufferEmittedCount ? (s.jitterBufferDelay / s.jitterBufferEmittedCount) * 1000 : null;
      out.width = s.frameWidth;
      out.height = s.frameHeight;
      out.fps = s.framesPerSecond;
    }
  });
  if (pair) {
    out.rttMs = (pair.currentRoundTripTime ?? 0) * 1000;
    const l = stats.get(pair.localCandidateId), r = stats.get(pair.remoteCandidateId);
    out.path = (l?.candidateType ?? "?") + " -> " + (r?.candidateType ?? "?");
  }
  pc.close();
  ws.close();
  return out;
}, POLICY);

const oneWay = result.rttMs ? result.rttMs / 2 : null;
const pipeline = [oneWay, result.jitterBufferAvgMs, result.decodeAvgMs].filter((x) => x != null).reduce((a, b) => a + b, 0);
console.log("RAW", JSON.stringify(result));
console.log(`network one-way ${oneWay?.toFixed(1)}ms + jitter buffer ${result.jitterBufferAvgMs?.toFixed(1)}ms + decode ${result.decodeAvgMs?.toFixed(2)}ms = ${pipeline.toFixed(1)}ms (capture+encode not included)`);
console.log("HINTS", JSON.stringify(result.hints), "(playoutDelayHint/jitterBufferTarget=0 reduces the jitter buffer)");
await browser.close();
