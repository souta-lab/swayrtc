// Standalone TURN reachability check.
// Forces the browser to use relay candidates only and reports whether media
// actually arrived. The host does not need TURN for this test: it learns the
// browser's relay candidate from signalling and opens the path by itself.
import { chromium } from "playwright-core";

const BASE = process.env.BASE || "http://127.0.0.1:8781";
const POLICY = process.env.POLICY || "relay";
const USER = process.env.USER_NAME || "swayrtctest";
const PASS = process.env.USER_PASS;
if (!PASS) { console.error("USER_PASS is not set (test account password: see .secrets/test-password)"); process.exit(1); }

const browser = await chromium.launch({
  executablePath: "/usr/bin/google-chrome",
  headless: true,
  args: ["--no-sandbox", "--autoplay-policy=no-user-gesture-required"],
});
const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
page.on("pageerror", (e) => console.log("PAGE_ERROR", e.message));

await page.goto(BASE + "/", { waitUntil: "domcontentloaded" });
await page.locator("#username").fill(USER);
await page.locator("#pass").fill(PASS);
await page.locator("#login button").click();
await page.waitForURL(BASE + "/", { timeout: 15000 });
// waitForURL resolves immediately because login and app share "/"; wait for the
// authenticated shell instead.
await page.waitForSelector("#screen", { state: "attached", timeout: 15000 });
await page.waitForFunction(async () => {
  const r = await fetch("/api/status", { credentials: "same-origin" });
  return r.ok;
}, undefined, { timeout: 15000 });
console.log("AUTHENTICATED");

const result = await page.evaluate(async (policy) => {
  const resp = await fetch("/api/ice", { credentials: "same-origin" });
  const text = await resp.text();
  let ice;
  try { ice = JSON.parse(text); } catch { return { status: resp.status, body: text.slice(0, 200), error: "non-json" }; }
  if (!ice.iceServers) return { status: resp.status, body: text.slice(0, 200), error: "no iceServers", cookie: document.cookie };
  const out = { iceServers: ice.iceServers?.length ?? 0, turn: !!ice.turn, candidates: [], selected: null, state: "", video: null, error: null };
  if (!out.iceServers) { out.error = "no ice servers"; return out; }
  const wsUrl = (location.protocol === "https:" ? "wss:" : "ws:") + "//" + location.host + "/ws/viewer";
  const pc = new RTCPeerConnection({ iceServers: ice.iceServers, iceTransportPolicy: policy, bundlePolicy: "max-bundle" });
  pc.addTransceiver("video", { direction: "recvonly" });
  const channel = pc.createDataChannel("control");
  window.__pc = pc;
  window.__channel = channel;
  const done = new Promise((resolve) => {
    const timer = setTimeout(() => resolve(), 45000);
    const finish = () => { clearTimeout(timer); resolve(); };
    pc.onconnectionstatechange = () => {
      out.state = pc.connectionState;
      if (pc.connectionState === "connected" || pc.connectionState === "failed") finish();
    };
    pc.onicecandidate = (e) => {
      if (e.candidate) { out.candidates.push(e.candidate.candidate.slice(0, 90)); ws.send(JSON.stringify({ type: "candidate", candidate: e.candidate.toJSON() })); }
    };
  });
  const ws = new WebSocket(wsUrl);
  await new Promise((resolve, reject) => { ws.onopen = resolve; ws.onerror = reject; });
  ws.onmessage = async (ev) => {
    const msg = JSON.parse(ev.data);
    if (msg.type === "answer") {
      await pc.setRemoteDescription(msg.sdp);
    } else if (msg.type === "candidate") {
      if (pc.remoteDescription) await pc.addIceCandidate(msg.candidate).catch(() => {});
    } else if (msg.type === "status" && msg.host) {
      const offer = await pc.createOffer();
      await pc.setLocalDescription(offer);
      ws.send(JSON.stringify({ type: "offer", sdp: pc.localDescription }));
    }
  };
  await done;
  await new Promise((r) => setTimeout(r, 4000));
  const stats = await pc.getStats();
  stats.forEach((s) => {
    if (s.type === "candidate-pair" && s.nominated && s.state === "succeeded") {
      const l = stats.get(s.localCandidateId), r = stats.get(s.remoteCandidateId);
      out.selected = { local: l?.candidateType, remote: r?.candidateType, rtt: s.currentRoundTripTime };
    }
  });
  stats.forEach((s) => {
    if (s.type === "inbound-rtp" && s.kind === "video") {
      out.video = { framesDecoded: s.framesDecoded, width: s.frameWidth, height: s.frameHeight };
      out.rtp = {};
      for (const k of ["framesReceived","framesDecoded","framesDropped","keyFramesDecoded","packetsReceived","packetsLost","jitter","bytesReceived","freezeCount","totalFreezesDuration","nackCount","pliCount","firCount","framesPerSecond"]) out.rtp[k] = s[k];
    }
  });
  ws.close();
  pc.close();
  return out;
}, POLICY);

console.log("RESULT", JSON.stringify(result, null, 1));
await browser.close();
