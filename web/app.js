/* SwayRTC viewer client.
 * No build step, no dependencies, CSP 'self' safe (no inline handlers/styles).
 * Sections: [state] [chrome] [signaling] [peer] [stats] [input] [keyboard] [viewport] [boot]
 */
(() => {
  "use strict";

  const $ = (id) => document.getElementById(id);
  const els = {
    stage: $("stage"), screen: $("screen"), stageBar: $("stage-bar"),
    empty: $("empty"), headline: $("headline"), detail: $("detail"), advice: $("advice"), overlayAction: $("overlay-action"),
    status: $("status"), statusLed: $("status-led"),
    chipSig: $("chip-sig"), chipIce: $("chip-ice"), chipDc: $("chip-dc"),
    peer: $("peer"), ice: $("ice"), gathering: $("gathering"), candidates: $("candidates"),
    transport: $("transport"), path: $("path"), turn: $("turn"),
    policy: $("policy"), policyLive: $("policy-live"), policyNote: $("policy-note"), advanced: $("advanced"),
    resolution: $("resolution"), fps: $("fps"), jitter: $("jitter"), codec: $("codec"), loss: $("loss"),
    rtt: $("rtt"), ping: $("ping"), sigrtt: $("sigrtt"), uptime: $("uptime"),
    sumLive: $("sum-live"), footerState: $("footer-state"),
    telemetry: $("telemetry"), notice: $("notice"),
    sheet: $("keyboard-panel"), textForm: $("text-form"), textInput: $("text-input"),
    kbModeText: $("kb-mode-text"), kbModeKeys: $("kb-mode-keys"), kbModeHint: $("kb-mode-hint"),
    control: $("control"), reconnect: $("reconnect"), reconnectTop: $("reconnect-top"),
    fullscreen: $("fullscreen"), keyboard: $("keyboard"), logout: $("logout"),
    lock: $("lock"), lockStage: $("lock-stage"),
    inputMode: $("input-mode"), kblock: $("kblock"), playout: $("playout"), hudLock: $("hud-lock"),
    rail: $("rail"), railToggle: $("rail-toggle"), railClose: $("rail-close"),
    hud: $("hud"), hudState: $("hud-state"), hudMetrics: $("hud-metrics"), hudLed: $("hud-led"), hudOpen: $("hud-open"),
    keybarKeys: $("keybar-keys"), fullKeyboard: $("full-keyboard"), exitFullscreen: $("exit-fullscreen")
  };

  /* ------------------------------------------------------------------ state */

  const MAX_ATTEMPTS = 5;
  const BACKOFF_MS = [1000, 2000, 4000, 8000, 16000];
  const ICE_SERVERS = [{ urls: ["stun:stun.cloudflare.com:3478", "stun:stun.l.google.com:19302"] }];
  const FALLBACK_SIZE = { w: 1920, h: 1080 };
  const POLICY_KEY = "swayrtc.icePolicy";
  // "all" = Auto: direct candidates first, TURN as the fallback. Relay-only is
  // still one click away for networks that cannot do direct at all.
  const POLICY_DEFAULT = "all";
  const RENDER_KEY = "swayrtc.renderer";
  const POLICIES = ["all", "relay", "direct"];
  // Only one viewer may hold the session, and a session that nobody drives is
  // torn down so the host stops capturing (and stops burning the encoder).
  const IDLE_TIMEOUT_MS = 30 * 60 * 1000;
  // A hidden tab keeps decoding and paying for media it cannot show. Drop the
  // connection after this grace period and reconnect when it comes back.
  // Brief tab switches are normal in remote-desktop use. A 5s timeout tore
  // down WebRTC on every jump between the remote screen and another tab.
  const BACKGROUND_PAUSE_MS = 60000;
  const IDLE_CHECK_MS = 15 * 1000;

  const POLICY_LABEL = { all: "Auto", relay: "TURN relay", direct: "Direct only" };
  const POLICY_NOTE = {
    all: "Direct UDP first, Cloudflare TURN as fallback. Use it to test whether a direct path works.",
    relay: "Media always flows through Cloudflare TURN. DTLS-SRTP stays end to end.",
    direct: "STUN only, no relay. Fails when both peers sit behind carrier-grade NAT."
  };

  const S = {
    ws: null, pc: null, dc: null, motion: null, buttonsCh: null, buttonSeq: 0,
    hostOnline: false,
    attempts: 0,
    retryTimer: 0, countdownTimer: 0,
    statsTimer: 0, watchdogTimer: 0, pingTimer: 0, sigTimer: 0, idleTimer: 0,
    lastInputAt: 0,
    backgroundTimer: 0,
    backgroundPaused: false,
    remoteCandidates: [],
    iceLocal: 0, iceRemote: 0, iceErrors: 0,
    peerStartedAt: 0, connectedAt: 0, disconnectedAt: 0,
    sigSentAt: 0, sigRtt: null,
    pingSamples: [], pingSent: 0,
    framePrev: null,
    pressed: new Set(), buttons: new Set(),
    pos: { x: 0, y: 0 }, hasPos: false,
    mods: { ctrl: false, alt: false, shift: false, super: false },
    kbMode: "text",
    mobile: false,
    stopping: false,
    icePolicy: "relay",
    peerSeq: 0,
    relayInUse: false,
    // Full lock mode: pointer lock supplies relative motion, keyboard lock
    // captures the keys Chrome would otherwise keep for itself.
    locked: false,
    escSwallowed: false,
    keyboardLock: "off",
    lowLatency: { playout: "ignored", jitter: "ignored" },
    // "element" paints through <video>; "webcodecs" decodes the RTP stream with
    // VideoDecoder and paints a canvas on top (input still goes through <video>).
    renderMode: "element",
    wc: null,
    // Per frame timing from requestVideoFrameCallback. The RTP receive clock and
    // the host clock are both wall clock on the reference machine, so these
    // samples can be lined up with the host's `frame.tx` log to split the total
    // latency into capture/encode, network, buffering and presentation.
    frameSamples: [],
    frameProbe: 0
  };

  const clamp = (v, lo, hi) => Math.max(lo, Math.min(hi, v));
  const pad2 = (n) => String(n).padStart(2, "0");
  const fmtMs = (ms) => (ms == null || !Number.isFinite(ms) ? "—" : Math.round(ms) + " ms");

  /* ----------------------------------------------------------------- chrome */

  function setStatus(text, led) {
    const klass = led === "on" ? "on" : led === "bad" ? "bad" : "";
    els.status.textContent = text;
    els.statusLed.className = klass;
    els.hudState.textContent = text;
    els.hudLed.className = klass;
  }

  function setChip(el, state) {
    if (!el) return;
    if (el.dataset.state !== state) el.dataset.state = state;
  }

  function setFooter(text) {
    clearTimeout(footerTimer);
    els.footerState.textContent = text;
  }

  let footerTimer = 0;
  function flashFooter(text, ms = 2200) {
    setFooter(text);
    footerTimer = setTimeout(idleFooter, ms);
  }

  function idleFooter() {
    if (S.dc && S.dc.readyState === "open") setFooter("Control channel open · input ready");
    else if (S.hostOnline) setFooter("Host online · waiting for media");
    else if (S.ws && S.ws.readyState === WebSocket.OPEN) setFooter("Waiting for the host agent");
    else setFooter("Signaling offline");
  }

  let noticeTimer = 0;
  function notice(text, ms = 4200) {
    clearTimeout(noticeTimer);
    if (!text) { els.notice.hidden = true; return; }
    els.notice.textContent = text;
    els.notice.hidden = false;
    noticeTimer = setTimeout(() => { els.notice.hidden = true; }, ms);
  }

  let overlayAction = null;
  function showOverlay({ headline, detail, advice = "", action = null }) {
    els.headline.textContent = headline;
    els.detail.textContent = detail || "";
    els.advice.textContent = advice || "";
    els.advice.hidden = !advice;
    overlayAction = action;
    els.overlayAction.hidden = !action;
    if (action) {
      els.overlayAction.textContent = action.label;
      els.overlayAction.setAttribute("aria-label", action.label);
    }
    els.empty.classList.remove("hidden");
  }

  function hideOverlay() {
    els.empty.classList.add("hidden");
  }

  els.overlayAction.addEventListener("click", () => {
    const action = overlayAction;
    if (action && typeof action.run === "function") action.run();
  });

  function resetRows() {
    for (const el of [els.peer, els.ice, els.gathering, els.candidates, els.transport, els.path, els.resolution,
      els.fps, els.jitter, els.codec, els.loss, els.rtt, els.ping]) {
      el.textContent = "—";
    }
    els.uptime.textContent = "—";
    els.sumLive.textContent = "—";
    els.hudMetrics.textContent = "—";
    if (els.playout) els.playout.textContent = "—";
  }

  /* -------------------------------------------------------------- signaling */

  function signal(packet) {
    if (S.ws && S.ws.readyState === WebSocket.OPEN) {
      try { S.ws.send(JSON.stringify(packet)); } catch { /* socket closed mid-send */ }
    }
  }

  function connect({ manual = false } = {}) {
    clearRetry();
    clearInterval(S.sigTimer);
    S.sigTimer = 0;
    S.peerSeq += 1;
    closePeer();
    if (manual) S.attempts = 0;
    if (S.ws) {
      S.ws.onopen = S.ws.onmessage = S.ws.onclose = S.ws.onerror = null;
      try { S.ws.close(); } catch { /* already gone */ }
      S.ws = null;
    }
    setChip(els.chipSig, "wait");
    setChip(els.chipIce, "idle");
    setChip(els.chipDc, "idle");
    els.sigrtt.textContent = "—";
    setStatus("Connecting", "wait");
    setFooter("Signaling connecting…");
    showOverlay({ headline: "Connecting to signaling", detail: "Opening an authenticated session socket." });

    const url = new URL("/ws/viewer", location.href);
    url.protocol = location.protocol === "https:" ? "wss:" : "ws:";
    const ws = new WebSocket(url);
    S.ws = ws;

    ws.onopen = () => {
      setChip(els.chipSig, "ok");
      setStatus("Signaling ready", "wait");
      setFooter("Signaling ready · waiting for the host");
      pingSignaling();
      S.sigTimer = setInterval(pingSignaling, 4000);
    };
    ws.onmessage = (ev) => { onSignal(ev).catch((err) => console.warn("signal handler", err)); };
    ws.onerror = () => setFooter("Signaling socket error");
    ws.onclose = (ev) => {
      clearInterval(S.sigTimer);
      S.sigTimer = 0;
      setChip(els.chipSig, "fail");
      setChip(els.chipIce, "idle");
      setChip(els.chipDc, "idle");
      els.sigrtt.textContent = "—";
      closePeer();
      if (S.stopping) return;
      if (ev.code === 4001) {
        setStatus("Session moved", "bad");
        setFooter("Another viewer took over this session");
        showOverlay({
          headline: "Session moved to another tab",
          detail: "This viewer was replaced by a newer connection.",
          advice: "Close the other tab, then reconnect.",
          action: { label: "Reconnect", run: () => connect({ manual: true }) }
        });
        return;
      }
      setStatus("Signaling lost", "bad");
      scheduleRetry("signaling");
    };
  }

  function pingSignaling() {
    S.sigSentAt = performance.now();
    signal({ type: "ping", at: Date.now() });
  }

  async function onSignal(ev) {
    let msg;
    try { msg = JSON.parse(ev.data); } catch { return; }
    if (!msg || typeof msg !== "object") return;

    switch (msg.type) {
      case "status": {
        const online = !!msg.host;
        const changed = online !== S.hostOnline;
        S.hostOnline = online;
        if (!online) {
          closePeer();
          setChip(els.chipIce, "idle");
          setChip(els.chipDc, "idle");
          setStatus("Host offline", "bad");
          setFooter("Waiting for the host agent");
          showOverlay({
            headline: "Waiting for host",
            detail: "The signaling server is up but no capture agent is connected.",
            advice: "Start swayrtc-host in the Sway session. This page reconnects on its own."
          });
        } else if (changed || !S.pc) {
          setFooter("Host online · negotiating media");
          startPeer();
        }
        break;
      }
      case "answer": {
        if (!S.pc) break;
        try {
          await S.pc.setRemoteDescription(msg.sdp);
          if (S.renderMode === "webcodecs") armWebCodecs(S.pc);
          for (const candidate of S.remoteCandidates.splice(0)) {
            await S.pc.addIceCandidate(candidate).catch(() => {});
          }
        } catch (err) {
          failPeer("Answer rejected", "The host sent an answer this browser could not apply.",
            String(err && err.message || err),
            "The host build may predate the current browser. Restart the host agent.");
        }
        break;
      }
      case "candidate": {
        if (!msg.candidate) break;
        S.iceRemote += 1;
        updateCandidateRow();
        if (S.pc && S.pc.remoteDescription) S.pc.addIceCandidate(msg.candidate).catch(() => {});
        else S.remoteCandidates.push(msg.candidate);
        break;
      }
      case "reset": {
        closePeer();
        setStatus("Host offline", "bad");
        setFooter("Host agent disconnected");
        showOverlay({
          headline: "Host disconnected",
          detail: "The capture agent closed the session.",
          advice: "Waiting for the host to come back. Retrying automatically."
        });
        break;
      }
      case "error": {
        const detail = String(msg.message || "unknown error").slice(0, 200);
        setStatus("Host error", "bad");
        setFooter("Host capture error");
        showOverlay({
          headline: "Host capture error",
          detail,
          advice: "Check the host agent log. Reconnect after fixing capture.",
          action: { label: "Reconnect", run: () => connect({ manual: true }) }
        });
        break;
      }
      case "pong": {
        if (S.sigSentAt) els.sigrtt.textContent = fmtMs(performance.now() - S.sigSentAt);
        break;
      }
      default:
        break;
    }
  }

  /* ------------------------------------------------------------ ICE config */

  // /api/ice is the single source of truth: STUN always, TURN credentials when
  // the server can mint them. A failed lookup degrades to STUN-only instead of
  // breaking the viewer.
  let icePromise = null;
  const iceState = { loaded: false, turn: null, degraded: false, at: 0, expiresAt: 0 };
  // TURN credentials are short lived (the server mints them with a 6h TTL).
  // Refresh at half life so a session that reconnects much later never keeps
  // trying to allocate with a credential that already expired.
  const ICE_REFRESH_FRACTION = 0.5;
  const ICE_FALLBACK_TTL_MS = 90000;

  function iceCacheFresh() {
    if (!icePromise || !iceState.expiresAt) return false;
    return Date.now() < iceState.expiresAt;
  }

  function invalidateIceConfig() {
    icePromise = null;
    iceState.expiresAt = 0;
    iceState.at = 0;
  }

  function loadIceConfig() {
    if (iceCacheFresh()) return icePromise;
    if (icePromise && !iceState.loaded) return icePromise;
    if (icePromise && iceState.loaded) {
      // Expired credentials are still better than nothing while we refetch.
      icePromise = null;
    }
    const now = Date.now();
    if (iceState.degraded && now - iceState.at < 30000) return Promise.resolve(ICE_SERVERS);
    icePromise = fetch("/api/ice", {
      headers: { accept: "application/json" },
      credentials: "same-origin",
      cache: "no-store"
    })
      .then((response) => {
        if (response.status === 401) {
          // The session cookie can still be settling right after login; retry once.
          return new Promise((resolve) => setTimeout(resolve, 700))
            .then(() => fetch("/api/ice", { headers: { accept: "application/json" }, credentials: "same-origin", cache: "no-store" }))
            .then((retry) => {
              if (!retry.ok) throw new Error("HTTP " + retry.status);
              return retry.json();
            });
        }
        if (!response.ok) throw new Error("HTTP " + response.status);
        return response.json();
      })
      .then((data) => {
        const servers = Array.isArray(data && data.iceServers)
          ? data.iceServers.filter((server) => server && server.urls)
          : [];
        if (!servers.length) throw new Error("empty iceServers");
        iceState.loaded = true;
        iceState.turn = !!data.turn;
        iceState.degraded = false;
        iceState.at = Date.now();
        const ttlMs = Number(data.ttl) > 0 ? Number(data.ttl) * 1000 : 0;
        iceState.expiresAt = ttlMs ? iceState.at + ttlMs * ICE_REFRESH_FRACTION : iceState.at + ICE_FALLBACK_TTL_MS;
        renderTurnRow();
        applyPolicyUi();
        return servers;
      })
      .catch((error) => {
        iceState.loaded = true;
        iceState.turn = false;
        iceState.degraded = true;
        iceState.at = Date.now();
        icePromise = null; // let a later attempt retry after the cooldown
        renderTurnRow();
        applyPolicyUi();
        console.warn("ICE config unavailable, using STUN only:", String(error && error.message || error));
        return ICE_SERVERS;
      });
    return icePromise;
  }

  // `window.__swayrtcForceRelay = true` (set before app.js runs) forces the
  // relay-only policy, for path diagnostics without touching the UI.
  const activePolicy = () => (window.__swayrtcForceRelay === true ? "relay" : S.icePolicy);

  const iceUrlList = (server) => (Array.isArray(server.urls) ? server.urls : [server.urls]);
  const isTurnEntry = (server) => iceUrlList(server).some((url) => /^turns?:/i.test(String(url)));

  function buildIceServers(servers) {
    // "Direct only" means: never offer relay candidates, so STUN is all we get.
    if (activePolicy() !== "direct") return servers;
    const direct = servers.filter((server) => !isTurnEntry(server));
    return direct.length ? direct : ICE_SERVERS;
  }

  function renderTurnRow() {
    const inUse = S.relayInUse;
    els.turn.dataset.state = inUse ? "relay" : "";
    if (inUse) { els.turn.textContent = "in use"; return; }
    if (!iceState.loaded) { els.turn.textContent = "—"; return; }
    if (iceState.degraded) { els.turn.textContent = "unavailable"; return; }
    els.turn.textContent = iceState.turn ? "available" : "none";
  }

  function applyPolicyUi() {
    // WebCodecs paints ~6 ms earlier than the media element, so it is the default
  // wherever the encoded-transform API exists; the toggle still forces either.
  const canWebCodecs = typeof RTCRtpScriptTransform !== "undefined" && typeof VideoDecoder !== "undefined";
  try { S.renderMode = sessionStorage.getItem(RENDER_KEY) || (canWebCodecs ? "webcodecs" : "element"); } catch { S.renderMode = canWebCodecs ? "webcodecs" : "element"; }
  function syncRenderUi() {
    for (const button of document.querySelectorAll("[data-render]")) {
      const on = button.dataset.render === S.renderMode;
      button.classList.toggle("is-on", on);
      button.setAttribute("aria-pressed", String(on));
    }
    renderNote(S.renderMode === "element"
      ? "Video element renderer. Switch to WebCodecs to decode the RTP stream directly onto a canvas."
      : "WebCodecs renderer: decoding straight from the RTP stream, painting a canvas over the video element.");
  }
  function setRenderMode(mode, { reconnect = true } = {}) {
    if (mode !== "element" && mode !== "webcodecs") return;
    if (S.renderMode === mode) return;
    S.renderMode = mode;
    try { sessionStorage.setItem(RENDER_KEY, mode); } catch { /* storage blocked */ }
    syncRenderUi();
    if (reconnect && S.ws) connect({ manual: true });
  }
  for (const button of document.querySelectorAll("[data-render]")) {
    button.addEventListener("click", () => setRenderMode(button.dataset.render));
  }
  syncRenderUi();
  window.__swayrtcRenderer = { mode: () => S.renderMode, set: (m) => setRenderMode(m), stats: () => (S.wc ? { drawn: S.wc.drawn, decodeDrawMs: S.wc.decodeDrawMs } : null) };

  for (const button of document.querySelectorAll("[data-policy]")) {
      const active = button.dataset.policy === S.icePolicy;
      button.classList.toggle("is-on", active);
      button.setAttribute("aria-pressed", String(active));
    }
    els.policy.textContent = POLICY_LABEL[S.icePolicy];
    els.policyLive.textContent = POLICY_LABEL[S.icePolicy];
    els.policyNote.textContent = S.icePolicy === "relay" && iceState.loaded && !iceState.turn
      ? "No TURN credentials are available (/api/ice), so relay-only cannot succeed."
      : POLICY_NOTE[S.icePolicy];
  }

  function setIcePolicy(policy, { reconnect = true, announce = true } = {}) {
    if (!POLICIES.includes(policy)) return;
    S.icePolicy = policy;
    try { sessionStorage.setItem(POLICY_KEY, policy); } catch { /* storage blocked */ }
    applyPolicyUi();
    if (announce) flashFooter("Path policy: " + POLICY_LABEL[policy], 2600);
    if (!reconnect) return;
    S.attempts = 0;
    clearRetry();
    if (S.ws && S.ws.readyState === WebSocket.OPEN && S.hostOnline) startPeer();
    else connect({ manual: true });
  }

  function policyDetail() {
    if (activePolicy() === "relay") return "TURN relay: waiting for Cloudflare relay candidates.";
    if (activePolicy() === "direct") return "Direct only: STUN candidates, relay fallback disabled.";
    return iceState.turn
      ? "Direct UDP preferred; Cloudflare TURN is the fallback."
      : "Direct UDP preferred; no TURN credentials are configured.";
  }

  function failureAdvice() {
    if (activePolicy() === "relay") {
      return "The TURN relay could not be reached: the /api/ice credential may have expired, DNS may fail, or turns:5349 / turn:3478 is blocked.";
    }
    if (activePolicy() === "direct") {
      return "Direct-only is on, so no relay was offered. Switch PATH POLICY to TURN relay to use Cloudflare TURN.";
    }
    if (iceState.degraded) {
      return "Direct UDP failed and /api/ice could not be reached, so no relay was offered. Retry when the server can mint TURN credentials.";
    }
    if (iceState.turn) {
      return "Direct UDP failed and the TURN relay did not answer either. The credential may have expired, or turns:5349 / turn:3478 is blocked here.";
    }
    return "Direct UDP is blocked and the server reported no TURN credentials (check .secrets/turn-key).";
  }

  function stallAdvice() {
    if (activePolicy() === "relay") return "The TURN relay is not answering. Verify the /api/ice credential and that turns:5349 is reachable.";
    if (activePolicy() === "direct") return "Direct-only mode has no fallback. Switch PATH POLICY to TURN relay.";
    if (iceState.turn) return "Neither the direct path nor the TURN relay completed. Retrying with backoff.";
    return "Direct UDP looks blocked and no TURN credentials are configured.";
  }


  /* ------------------------------------------------------------------ peer */

  async function startPeer() {
    if (!S.ws || S.ws.readyState !== WebSocket.OPEN || !S.hostOnline) return;
    const seq = ++S.peerSeq;
    closePeer();
    S.peerStartedAt = performance.now();
    setChip(els.chipIce, "wait");
    setStatus("Negotiating", "wait");
    setFooter("Gathering ICE candidates");
    showOverlay({
      headline: activePolicy() === "relay" ? "Negotiating a relay path" : "Negotiating a direct path",
      detail: policyDetail()
    });

    // Credentials come from /api/ice; a failure degrades to STUN-only.
    const servers = await loadIceConfig();
    if (seq !== S.peerSeq) return;
    if (!S.ws || S.ws.readyState !== WebSocket.OPEN || !S.hostOnline || S.pc) return;
    els.detail.textContent = policyDetail();

    let pc;
    try {
      pc = new RTCPeerConnection({
        iceServers: buildIceServers(servers),
        iceTransportPolicy: activePolicy() === "relay" ? "relay" : "all",
        bundlePolicy: "max-bundle"
      });
    } catch (err) {
      failPeer("WebRTC unavailable", "This browser refused to create a peer connection.",
        String(err && err.message || err), "Use a current Chrome, Edge, Firefox or Safari.");
      return;
    }
    S.pc = pc;
    pc.addTransceiver("video", { direction: "recvonly" });
    // The transform must be installed before the receiver starts carrying frames.
    if (S.renderMode === "webcodecs") armWebCodecs(pc);

    // Keys, buttons, wheel and text must not be lost or reordered: a lost keyup
    // leaves the key held down and the compositor auto-repeats it (a tap turns
    // into a burst), a lost keydown drops a character. Pointer motion is the
    // opposite: newest wins, dropping a stale move is harmless. Two channels,
    // two policies.
    const dc = pc.createDataChannel("control", { ordered: true });
    S.dc = dc;
    const motion = pc.createDataChannel("motion", { ordered: false, maxRetransmits: 0 });
    S.motion = motion;
    // Buttons: reliable (a click must not be lost) but unordered, so a
    // retransmission cannot hold up the next click. The host drops stale
    // sequence numbers, so reordering cannot leave a button stuck.
    const buttons = pc.createDataChannel("buttons", { ordered: false });
    S.buttonsCh = buttons;
    S.buttonSeq = 0;
    setChip(els.chipDc, "wait");
    dc.onopen = () => {
      setChip(els.chipDc, "ok");
      setFooter("Control channel open · input ready");
      startChannelPing();
    };
    dc.onclose = () => {
      setChip(els.chipDc, "idle");
      stopChannelPing();
      els.ping.textContent = "—";
    };
    dc.onmessage = (ev) => {
      S.pingSent = 0;
      let msg;
      try { msg = JSON.parse(ev.data); } catch { return; }
      if (msg && msg.type === "pong" && Number.isFinite(msg.t)) {
        const rtt = performance.now() - msg.t;
        S.pingSamples.push(rtt);
        if (S.pingSamples.length > 6) S.pingSamples.shift();
        const avg = S.pingSamples.reduce((a, b) => a + b, 0) / S.pingSamples.length;
        els.ping.textContent = fmtMs(avg);
        els.ping.title = "DataChannel ping/pong round trip (" + S.pingSamples.length + " samples)";
      }
    };

    pc.ontrack = (ev) => {
      if (ev.track.kind !== "video") return;
      // createEncodedStreams has to run before anything starts the receiver
      // (attaching srcObject counts), otherwise Chrome answers
      // "Too late to create encoded streams".
      if (S.renderMode === "webcodecs") startWebCodecs(ev.receiver);
      tuneReceiver(ev.receiver);
      els.screen.srcObject = ev.streams[0] || new MediaStream([ev.track]);
      document.body.dataset.live = "on";
      playVideo();
      startFrameProbe();
      els.screen.focus({ preventScroll: true });
    };
    pc.onicecandidate = (ev) => {
      if (!ev.candidate) return;
      S.iceLocal += 1;
      updateCandidateRow();
      signal({ type: "candidate", candidate: ev.candidate.toJSON() });
    };
    pc.onicecandidateerror = () => { S.iceErrors += 1; };
    pc.onicegatheringstatechange = renderPeer;
    pc.oniceconnectionstatechange = renderPeer;
    pc.onconnectionstatechange = renderPeer;
    pc.onsignalingstatechange = renderPeer;

    renderPeer();
    startWatchdog();

    pc.createOffer({ offerToReceiveVideo: true })
      .then((offer) => pc.setLocalDescription(offer))
      .then(() => { signal({ type: "offer", sdp: pc.localDescription }); })
      .catch((err) => {
        failPeer("Offer failed", "The browser could not build a media offer.",
          String(err && err.message || err), "Reload the page, then reconnect.");
      });
  }

  function renderPeer() {
    const pc = S.pc;
    if (!pc) return;
    const iceState = pc.iceConnectionState;
    const connState = pc.connectionState;
    els.peer.textContent = connState || "—";
    els.ice.textContent = iceState;
    els.gathering.textContent = pc.iceGatheringState;

    if (iceState === "connected" || iceState === "completed") setChip(els.chipIce, "ok");
    else if (iceState === "failed") setChip(els.chipIce, "fail");
    else setChip(els.chipIce, "wait");

    if (iceState === "connected" || iceState === "completed") {
      if (!S.connectedAt) {
        S.connectedAt = performance.now();
        startIdleWatch();
        S.attempts = 0;
        S.disconnectedAt = 0;
        setStatus("Connected", "on");
        setFooter("Direct encrypted path established");
        startStats();
      }
      if (els.screen.readyState >= 2 && !els.screen.paused) hideOverlay();
    }

    if (connState === "failed" || iceState === "failed") {
      // A relay that cannot allocate is the usual failure mode once the minted
      // TURN credential expires, so drop the cache and let the retry refetch.
      if (activePolicy() !== "direct") invalidateIceConfig();
      const headline = activePolicy() === "relay" ? "Relay path failed" : "ICE path failed";
      failPeer(headline,
        "No candidate pair completed (" + S.iceLocal + " sent / " + S.iceRemote + " received, " + S.iceErrors + " errors).",
        failureAdvice(),
        { retry: true });
      return;
    }

    if (iceState === "disconnected" && S.connectedAt) {
      setStatus("Interrupted", "bad");
      setFooter("ICE interrupted · probing a new path");
      setChip(els.chipIce, "fail");
    }
  }

  function failPeer(headline, detail, advice, options = {}) {
    setStatus("Connection failed", "bad");
    setChip(els.chipIce, "fail");
    closePeer();
    showOverlay({
      headline,
      detail,
      advice,
      action: { label: "Retry now", run: () => connect({ manual: true }) }
    });
    if (options.retry !== false) scheduleRetry("media");
  }

  function closePeer() {
    stopStats();
    stopChannelPing();
    stopIdleWatch();
    stopFrameProbe();
    stopWebCodecs();
    clearInterval(S.statsTimer);
    S.statsTimer = 0;
    S.connectedAt = 0;
    S.disconnectedAt = 0;
    S.peerStartedAt = 0;
    S.framePrev = null;
    S.iceLocal = 0;
    S.iceRemote = 0;
    S.iceErrors = 0;
    S.remoteCandidates.length = 0;
    S.pressed.clear();
    S.buttons.clear();
    S.pingSamples.length = 0;
    S.pingSent = 0;

    const dc = S.dc;
    if (dc) {
      dc.onopen = dc.onclose = dc.onmessage = null;
      try { dc.close(); } catch { /* already closed */ }
      S.dc = null;
      S.motion = null;
      S.buttonsCh = null;
    }
    const pc = S.pc;
    if (pc) {
      pc.ontrack = pc.onicecandidate = pc.onicecandidateerror = null;
      pc.onicegatheringstatechange = pc.oniceconnectionstatechange = null;
      pc.onconnectionstatechange = pc.onsignalingstatechange = null;
      try { pc.close(); } catch { /* already closed */ }
      S.pc = null;
    }
    delete document.body.dataset.live;
    if (els.screen.srcObject) els.screen.srcObject = null;
    setChip(els.chipDc, "idle");
    S.relayInUse = false;
    resetRows();
    renderTurnRow();
    stopWatchdog();
  }

  /* ------------------------------------------------------------ retry logic */

  function clearRetry() {
    clearTimeout(S.retryTimer);
    S.retryTimer = 0;
    clearInterval(S.countdownTimer);
    S.countdownTimer = 0;
  }

  function scheduleRetry(kind) {
    clearRetry();
    if (S.stopping) return;
    S.attempts += 1;
    if (S.attempts > MAX_ATTEMPTS) {
      setStatus("Offline", "bad");
      setFooter("Automatic reconnect stopped after " + MAX_ATTEMPTS + " attempts");
      showOverlay({
        headline: "Could not reconnect",
        detail: "Gave up after " + MAX_ATTEMPTS + " attempts with exponential backoff.",
        advice: "Check the host agent and network, then reconnect manually.",
        action: { label: "Reconnect", run: () => connect({ manual: true }) }
      });
      return;
    }
    const delay = BACKOFF_MS[Math.min(S.attempts - 1, BACKOFF_MS.length - 1)];
    const until = performance.now() + delay;
    const signaling = kind === "signaling";
    showOverlay({
      headline: signaling ? "Signaling interrupted" : "Media path interrupted",
      detail: "Re-establishing the " + (signaling ? "authenticated session socket" : "direct WebRTC path") + ".",
      advice: "Retries back off automatically, up to " + MAX_ATTEMPTS + " attempts."
    });
    const report = () => {
      const left = Math.max(0, Math.ceil((until - performance.now()) / 1000));
      setStatus("Retry " + S.attempts + "/" + MAX_ATTEMPTS + " in " + left + "s", "wait");
      setFooter("Retry " + S.attempts + "/" + MAX_ATTEMPTS + " in " + left + "s (" + kind + ")");
      els.detail.textContent = "Attempt " + S.attempts + "/" + MAX_ATTEMPTS + " fires in " + left + "s (" + kind + ").";
    };
    report();
    S.countdownTimer = setInterval(report, 250);
    S.retryTimer = setTimeout(() => {
      clearRetry();
      if (kind === "signaling") connect();
      else startPeer();
    }, delay);
  }

  function startWatchdog() {
    if (S.watchdogTimer) return;
    S.watchdogTimer = setInterval(watchdog, 1000);
  }

  function stopWatchdog() {
    clearInterval(S.watchdogTimer);
    S.watchdogTimer = 0;
  }

  function watchdog() {
    const pc = S.pc;
    if (!pc) return;
    const now = performance.now();

    if (!S.connectedAt && S.peerStartedAt) {
      const elapsed = now - S.peerStartedAt;
      const pending = pc.iceConnectionState === "new" || pc.iceConnectionState === "checking";
      if (pending && elapsed > 12000) {
        setStatus("Negotiation stalled", "bad");
        setChip(els.chipIce, "fail");
        showOverlay({
          headline: "Negotiation stalled",
          detail: "ICE stayed " + pc.iceConnectionState + " for " + Math.round(elapsed / 1000) + " s (" +
            S.iceLocal + " candidates sent, " + S.iceRemote + " received).",
          advice: stallAdvice()
        });
        closePeer();
        scheduleRetry("media");
        return;
      }
    }

    if (pc.iceConnectionState === "disconnected" && S.connectedAt) {
      if (!S.disconnectedAt) {
        S.disconnectedAt = now;
        try { pc.restartIce(); } catch { /* not supported, watchdog restarts below */ }
      } else if (now - S.disconnectedAt > 7000) {
        closePeer();
        scheduleRetry("media");
      }
    } else if (pc.iceConnectionState === "connected") {
      S.disconnectedAt = 0;
    }

    if (S.connectedAt) {
      const total = Math.floor((now - S.connectedAt) / 1000);
      els.uptime.textContent = pad2(Math.floor(total / 60)) + ":" + pad2(total % 60);
    }
  }

  /* ---------------------------------------------------------- low latency */

  // Both knobs are implementation extras rather than baseline WebRTC: probe the
  // receiver instead of assuming, and report what actually stuck.
  /* ------------------------------------------------- WebCodecs renderer */

  function renderNote(text) {
    const el = $("render-note");
    if (el) el.textContent = text;
  }

  function layoutWebCodecs() {
    const wc = S.wc;
    if (!wc) return;
    const video = els.screen;
    const stageEl = video.parentElement;
    if (!stageEl) return;
    const r = video.getBoundingClientRect();
    const sr = stageEl.getBoundingClientRect();
    const vw = video.videoWidth, vh = video.videoHeight;
    if (!vw || !vh || !r.width || !r.height) return;
    const aspect = Math.min(r.width / vw, r.height / vh);
    const w = vw * aspect, h = vh * aspect;
    wc.canvas.style.left = r.left - sr.left + (r.width - w) / 2 + "px";
    wc.canvas.style.top = r.top - sr.top + (r.height - h) / 2 + "px";
    wc.canvas.style.width = w + "px";
    wc.canvas.style.height = h + "px";
  }

  function stopWebCodecs() {
    const wc = S.wc;
    S.wc = null;
    if (!wc) return;
    try { wc.worker.postMessage({ stop: true }); } catch { /* worker gone */ }
    try { wc.worker.terminate(); } catch { /* already gone */ }
    try { wc.channel.port1.close(); } catch { /* already closed */ }
    try { if (wc.canvas) wc.canvas.remove(); } catch { /* already gone */ }
  }

  /// Install the encoded transform on the video receiver. Must happen before the
  /// receiver starts, so it is armed right after the remote description is set.
  function armWebCodecs(pc) {
    if (!pc || typeof RTCRtpScriptTransform === "undefined") return;
    const receiver = pc.getReceivers().find((r) => r.track && r.track.kind === "video") || pc.getReceivers()[0];
    if (!receiver) return;
    if (S.wc && S.wc.pc === pc) return;
    stopWebCodecs();
    let worker;
    try {
      worker = new Worker("/web/transform-worker.js");
    } catch (err) {
      renderNote("Worker blocked: " + String(err && err.message || err).slice(0, 80));
      return;
    }
    const channel = new MessageChannel();
    const state = { pc, worker, channel, canvas: null, ctx: null, drawn: 0, decodeDrawMs: null };
    channel.port1.onmessage = (event) => {
      const msg = event.data || {};
      if (msg.type === "stats") {
        window.__swayrtcWcStats = msg.stats;
        return;
      }
      if (msg.type === "diag") {
        return; // diagnostics are opt-in via the console
      }
      if (msg.type === "error") {
        renderNote("Decoder error: " + String(msg.message || "").slice(0, 80));
        return;
      }
      if (msg.type !== "frame" || !msg.frame) return;
      if (!state.canvas) return;
      const frame = msg.frame;
      const canvas = state.canvas;
      if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
        canvas.width = frame.displayWidth;
        canvas.height = frame.displayHeight;
      }
      const t0 = performance.now();
      state.ctx.drawImage(frame, 0, 0);
      state.decodeDrawMs = t0 ? performance.now() - t0 : null;
      frame.close();
      state.drawn += 1;
      layoutWebCodecs();
    };
    worker.postMessage({ port: channel.port2 }, [channel.port2]);
    try {
      receiver.transform = new RTCRtpScriptTransform(worker, { operation: "decode" });
    } catch (err) {
      try { worker.terminate(); } catch { /* already gone */ }
      renderNote("Transform rejected: " + String(err && err.message || err).slice(0, 80));
      return;
    }
    S.wc = state;
    renderNote("WebCodecs armed");
  }

  /// Called from ontrack: adds the canvas that the transform paints into.
  function attachWebCodecsCanvas() {
    const state = S.wc;
    if (!state || state.canvas) return;
    const canvas = document.createElement("canvas");
    canvas.className = "wc-surface";
    els.screen.parentElement.appendChild(canvas);
    state.canvas = canvas;
    state.ctx = canvas.getContext("2d", { alpha: false, desynchronized: true });
    layoutWebCodecs();
    renderNote("WebCodecs decoding");
  }

  function startWebCodecs(receiver) {
    if (S.renderMode !== "webcodecs") return;
    if (!S.wc) armWebCodecs(S.pc);
    attachWebCodecsCanvas();
  }

  function tuneReceiver(receiver) {
    const applied = { playout: "ignored", jitter: "ignored" };
    if (receiver) {
      try {
        if ("playoutDelayHint" in receiver) {
          receiver.playoutDelayHint = 0;
          applied.playout = "applied";
        }
      } catch { /* the implementation refused the value */ }
      try {
        if ("jitterBufferTarget" in receiver) {
          receiver.jitterBufferTarget = 0;
          applied.jitter = "applied";
        }
      } catch { /* the implementation refused the value */ }
    }
    S.lowLatency = applied;
    renderLowLatencyRow();
  }

  function renderLowLatencyRow() {
    if (!els.playout) return;
    const { playout, jitter } = S.lowLatency;
    const any = playout === "applied" || jitter === "applied";
    els.playout.textContent = any ? "applied" : "ignored";
    els.playout.title = "playoutDelayHint: " + playout + " · jitterBufferTarget: " + jitter;
  }

  /* ----------------------------------------------------------------- stats */

  function updateCandidateRow() {
    els.candidates.textContent = S.iceLocal + " → " + S.iceRemote;
  }

  function startStats() {
    if (S.statsTimer) return;
    S.statsTimer = setInterval(updateStats, 1000);
    updateStats();
  }

  function stopStats() {
    clearInterval(S.statsTimer);
    S.statsTimer = 0;
  }

  async function updateStats() {
    const pc = S.pc;
    if (!pc) return;
    let results;
    try { results = await pc.getStats(); } catch { return; }
    const byId = new Map();
    results.forEach((r) => byId.set(r.id, r));

    let transport = null;
    let pair = null;
    let inbound = null;
    results.forEach((r) => {
      if (r.type === "transport") transport = r;
      if (r.type === "inbound-rtp" && r.kind === "video") inbound = r;
    });
    if (transport && transport.selectedCandidatePairId) pair = byId.get(transport.selectedCandidatePairId);
    if (!pair) {
      results.forEach((r) => {
        if (r.type === "candidate-pair" && r.state === "succeeded" && (r.nominated || r.selected)) pair = r;
      });
    }

    if (pair) {
      const local = byId.get(pair.localCandidateId);
      const remote = byId.get(pair.remoteCandidateId);
      const localType = local?.candidateType || "?";
      const remoteType = remote?.candidateType || "?";
      const relay = localType === "relay" || remoteType === "relay";
      const protocol = String(pair.protocol || local?.protocol || "udp").toUpperCase();
      // candidate types: host / srflx / prflx / relay
      const relayPolicy = activePolicy() === "relay";
      const route = relay ? (relayPolicy ? "RELAY" : "RELAY (fallback)") : "DIRECT";
      els.transport.textContent = protocol + " · " + route;
      els.transport.dataset.state = relay ? "relay" : "direct";
      els.path.textContent = localType + " → " + remoteType;
      els.path.title = (local?.address ? String(local.address).replace(/:\d+$/, "") + " (" + localType + ") → " : "") +
        (remote?.address ? String(remote.address).replace(/:\d+$/, "") + " (" + remoteType + ")" : "");
      els.rtt.textContent = fmtMs(pair.currentRoundTripTime != null ? pair.currentRoundTripTime * 1000 : null);
      if (relay !== S.relayInUse) {
        S.relayInUse = relay;
        renderTurnRow();
        const connected = pc.iceConnectionState === "connected" || pc.iceConnectionState === "completed";
        if (connected) {
          setStatus(relay ? (relayPolicy ? "TURN relay" : "TURN relay (fallback)") : "Connected", "on");
          if (relay) setFooter("Media relayed by Cloudflare TURN · DTLS-SRTP stays end to end");
          else idleFooter();
        }
      }
      els.policy.textContent = POLICY_LABEL[activePolicy()] + (window.__swayrtcForceRelay === true ? " · hook" : "");
    }

    if (inbound) {
      if (inbound.frameWidth && inbound.frameHeight) {
        els.resolution.textContent = inbound.frameWidth + " × " + inbound.frameHeight;
      }
      let fps = Number.isFinite(inbound.framesPerSecond) && inbound.framesPerSecond > 0
        ? inbound.framesPerSecond
        : null;
      const now = inbound.timestamp || performance.now();
      const frames = inbound.framesDecoded || 0;
      if (S.framePrev) {
        const dt = (now - S.framePrev.time) / 1000;
        if (dt > 0.2) {
          const measured = (frames - S.framePrev.frames) / dt;
          if (!Number.isFinite(fps) || fps == null) fps = measured;
          if (fps < 0) fps = 0;
        }
      }
      S.framePrev = { frames, time: now };
      if (fps != null && Number.isFinite(fps)) els.fps.textContent = fps.toFixed(1);

      const codec = inbound.codecId ? byId.get(inbound.codecId) : null;
      const codecName = codec?.mimeType ? String(codec.mimeType).split("/").pop() : "—";
      els.codec.textContent = codecName;

      // RTP inter-arrival jitter: Chrome reports seconds, some builds report
      // timestamp units — normalise with the codec clock rate when it looks raw.
      let jitterMs = null;
      if (inbound.jitter != null) {
        jitterMs = inbound.jitter > 1
          ? (inbound.jitter / (codec?.clockRate || 90000)) * 1000
          : inbound.jitter * 1000;
      } else if (inbound.jitterBufferDelay != null && inbound.jitterBufferEmittedCount) {
        jitterMs = (inbound.jitterBufferDelay / inbound.jitterBufferEmittedCount) * 1000;
      }
      els.jitter.textContent = jitterMs == null ? "—" : jitterMs.toFixed(1) + " ms";

      const lost = inbound.packetsLost;
      els.loss.textContent = lost == null ? "—" : String(Math.max(0, lost));

      const summary = [els.resolution.textContent, els.fps.textContent !== "—" ? els.fps.textContent + " fps" : null,
        S.relayInUse ? (activePolicy() === "relay" ? "TURN" : "TURN fallback") : null]
        .filter(Boolean).join(" · ");
      if (summary) {
        els.sumLive.textContent = summary;
        els.hudMetrics.textContent = summary;
      }
    }
  }

  /* ---------------------------------------------------------- channel ping */

  function startChannelPing() {
    if (S.pingTimer) return;
    S.pingTimer = setInterval(() => {
      if (!S.dc || S.dc.readyState !== "open") return;
      S.pingSent += 1;
      if (S.pingSent > 3 && !S.pingSamples.length) {
        els.ping.textContent = "no reply";
        els.ping.title = "The host build does not answer DataChannel pings yet";
      }
      try { S.dc.send(JSON.stringify({ type: "ping", t: performance.now() })); } catch { /* channel closing */ }
    }, 2000);
  }

  function stopChannelPing() {
    clearInterval(S.pingTimer);
    S.pingTimer = 0;
  }

  /* --------------------------------------------------------------- control */

  function canSend() {
    return els.control.checked && S.dc != null && S.dc.readyState === "open";
  }

  // A tiny, privacy-preserving input health probe. This records counters only,
  // never keys, coordinates or screen content. Inspect through the JS console
  // if clicks still go missing after a tab switch.
  const pointerHealth = { down: 0, up: 0, sent: 0, blocked: 0, cancels: 0, last: "none" };
  window.__swayrtcInputHealth = () => ({
    ...pointerHealth,
    channel: S.dc?.readyState || "none",
    motionChannel: S.motion?.readyState || "none",
    controlEnabled: els.control.checked,
    backgroundPaused: S.backgroundPaused,
    locked: S.locked,
    peer: S.pc?.connectionState || "none"
  });

  const FRAME_SAMPLE_MAX = 600;

  function stopFrameProbe() {
    S.frameProbe = 0;
    S.frameSamples.length = 0;
  }

  function startFrameProbe() {
    stopFrameProbe();
    const video = els.screen;
    if (!video || typeof video.requestVideoFrameCallback !== "function") return;
    const tick = (now, meta) => {
      if (!S.pc) return;
      if (S.frameSamples.length < FRAME_SAMPLE_MAX) {
        S.frameSamples.push({
          now,
          receiveTime: meta.receiveTime ?? null,
          expectedDisplayTime: meta.expectedDisplayTime ?? null,
          presentationTime: meta.presentationTime ?? null,
          processingMs: meta.processingDuration != null ? meta.processingDuration * 1000 : null,
          presentedFrames: meta.presentedFrames ?? null,
          width: meta.width ?? null,
          height: meta.height ?? null,
        });
      }
      S.frameProbe = video.requestVideoFrameCallback(tick);
    };
    S.frameProbe = video.requestVideoFrameCallback(tick);
  }

  /** Compact percentile summary of the sampled frame timings. */
  function frameProbeSummary() {
    const s = S.frameSamples;
    if (!s.length) return null;
    const pick = (values) => {
      const v = values.filter((x) => x != null).sort((a, b) => a - b);
      if (!v.length) return null;
      return { p50: v[Math.floor(v.length * 0.5)], p90: v[Math.floor(v.length * 0.9)], n: v.length };
    };
    return {
      samples: s.length,
      processingMs: pick(s.map((x) => x.processingMs)),
      displayDelayMs: pick(s.map((x) => (x.expectedDisplayTime != null && x.now != null ? x.expectedDisplayTime - x.now : null))),
      receiveToNowMs: pick(s.map((x) => (x.receiveTime != null ? x.now - x.receiveTime : null))),
      raw: s,
    };
  }
  // Exposed for tools/latency-breakdown.mjs; harmless in production.
  window.__swayrtcPerf = { summary: frameProbeSummary, reset: () => { S.frameSamples.length = 0; } };

  function stopIdleWatch() {
    clearInterval(S.idleTimer);
    S.idleTimer = 0;
  }

  function suspendForBackground() {
    if (S.backgroundPaused || !S.connectedAt) return;
    S.backgroundPaused = true;
    // Same teardown as the idle timeout, minus the overlay: the host stops its
    // capture child as soon as the viewer disappears, so this saves both the
    // uplink and the encoder.
    S.stopping = true;
    closePeer();
    if (S.ws) {
      try { S.ws.close(4003, "background"); } catch { /* already gone */ }
      S.ws = null;
    }
    setStatus("Paused", "wait");
    setChip(els.chipSig, "idle");
    setChip(els.chipIce, "idle");
    setChip(els.chipDc, "idle");
    setFooter("Tab hidden · media paused to save bandwidth");
    showOverlay({
      headline: "Paused in the background",
      detail: "The stream was stopped while this tab is hidden so it does not use bandwidth or the host encoder.",
      advice: "Switch back to this tab to resume automatically.",
      action: { label: "Resume", run: () => resumeFromBackground() }
    });
  }

  function resumeFromBackground() {
    if (!S.backgroundPaused) return;
    S.backgroundPaused = false;
    S.stopping = false;
    setStatus("Connecting", "wait");
    connect({ manual: true });
  }

  function onVisibilityChange() {
    clearTimeout(S.backgroundTimer);
    S.backgroundTimer = 0;
    if (document.hidden) {
      releaseAll();
      S.backgroundTimer = setTimeout(suspendForBackground, BACKGROUND_PAUSE_MS);
      return;
    }
    if (S.backgroundPaused) resumeFromBackground();
  }

  function startIdleWatch() {
    stopIdleWatch();
    S.lastInputAt = Date.now();
    S.idleTimer = setInterval(() => {
      if (!S.connectedAt) return;
      if (Date.now() - S.lastInputAt < IDLE_TIMEOUT_MS) return;
      endIdleSession();
    }, IDLE_CHECK_MS);
  }

  function endIdleSession() {
    stopIdleWatch();
    const minutes = Math.round(IDLE_TIMEOUT_MS / 60000);
    // Suppress the retry that the socket close handler would otherwise schedule.
    S.stopping = true;
    closePeer();
    if (S.ws) {
      try { S.ws.close(4003, "idle timeout"); } catch { /* already gone */ }
      S.ws = null;
    }
    setChip(els.chipSig, "idle");
    setChip(els.chipIce, "idle");
    setChip(els.chipDc, "idle");
    setStatus("Session closed", "bad");
    setFooter("Closed after " + minutes + " min without input");
    showOverlay({
      headline: "Session closed after " + minutes + " minutes idle",
      detail: "No input was sent for " + minutes + " minutes, so the remote session was released and the host stopped capturing.",
      advice: "Reconnect to resume. Nothing was lost on the remote desktop.",
      action: {
        label: "Reconnect",
        run: () => { S.stopping = false; connect({ manual: true }); }
      }
    });
  }

  function sendMotion(message) {
    // A previously open motion channel is not proof that the reliable control
    // channel is ready. Never show a moving cursor while clicks cannot be sent.
    if (!canSend()) return false;
    const ch = S.motion && S.motion.readyState === "open" ? S.motion : S.dc;
    if (!ch) return false;
    try {
      ch.send(JSON.stringify({ ...message, t: Date.now() }));
      S.lastInputAt = Date.now();
      return true;
    } catch {
      return false;
    }
  }

  function sendControl(message) {
    if (!canSend()) {
      if (message.type === "button") {
        pointerHealth.blocked += 1;
        pointerHealth.last = "blocked:" + (els.control.checked ? (S.dc?.readyState || "no channel") : "input disabled");
        flashFooter("Click blocked · " + (els.control.checked ? "connecting / control channel closed" : "remote input switched off"), 2200);
      }
      return false;
    }
    try {
      // Stamp every input message: the host reports (now - t) so the input leg
      // can be measured end to end without any clock assumptions on the client.
      S.dc.send(JSON.stringify({ ...message, t: Date.now() }));
      S.lastInputAt = Date.now();
      if (message.type === "button") {
        pointerHealth.sent += 1;
        pointerHealth.last = message.down ? "press sent" : "release sent";
      }
      return true;
    } catch {
      if (message.type === "button") {
        pointerHealth.blocked += 1;
        pointerHealth.last = "send error";
        flashFooter("Click delivery failed · control channel error", 2200);
      }
      return false;
    }
  }

  function modifierState(extra) {
    return {
      ctrlKey: !!(S.mods.ctrl || (extra && extra.ctrlKey)),
      altKey: !!(S.mods.alt || (extra && extra.altKey)),
      shiftKey: !!(S.mods.shift || (extra && extra.shiftKey)),
      metaKey: !!(S.mods.super || (extra && extra.metaKey))
    };
  }

  // A latched modifier is a one-shot: it is spent by the next real key. Sending
  // a modifier key itself (physical Ctrl, Meta/Search) is not a key press in that
  // sense, so those codes never clear the latches.
  const MODIFIER_CODES = new Set([
    "ShiftLeft", "ShiftRight", "ControlLeft", "ControlRight",
    "AltLeft", "AltRight", "MetaLeft", "MetaRight",
    "OSLeft", "OSRight", "SuperLeft", "SuperRight"
  ]);

  const latchedModifiers = () => Object.entries(S.mods).filter(([, on]) => on).map(([name]) => name);

  function syncModUi() {
    for (const button of document.querySelectorAll("[data-mod]")) {
      const name = button.dataset.mod;
      if (!(name in S.mods)) continue;
      button.setAttribute("aria-pressed", String(!!S.mods[name]));
    }
  }

  function spendModifierLatch() {
    if (!latchedModifiers().length) return false;
    S.mods = { ctrl: false, alt: false, shift: false, super: false };
    syncModUi();
    return true;
  }

  function sendKey(code, key, down, extra) {
    const sent = sendControl(Object.assign({
      type: "key", code, key, down, repeat: !!(extra && extra.repeat)
    }, modifierState(extra)));
    // Spend the latch *after* the key-down that used it, so the key-up goes out
    // plain and the host releases the modifier instead of leaving it stuck.
    if (sent && down && !(extra && extra.repeat) && !MODIFIER_CODES.has(code)) spendModifierLatch();
    return sent;
  }

  function releaseAll() {
    if (S.dc && S.dc.readyState === "open") {
      for (const code of S.pressed) {
        S.dc.send(JSON.stringify(Object.assign({
          type: "key", code, key: keyFromCode(code), down: false, repeat: false
        }, { ctrlKey: false, altKey: false, shiftKey: false, metaKey: false })));
      }
      for (const button of S.buttons) {
        // While locked the host pointer already sits wherever the relative
        // motion left it; sending a stale absolute position would yank it back.
        const payload = S.locked
          ? { type: "button", button, down: false }
          : { type: "button", button, down: false, x: S.pos.x, y: S.pos.y };
        sendButton(payload);
      }
    }
    S.pressed.clear();
    S.buttons.clear();
  }

  function keyFromCode(code) {
    if (code.startsWith("Key")) return code.slice(3).toLowerCase();
    if (code.startsWith("Digit")) return code.slice(5);
    if (code === "Space") return " ";
    const named = {
      Escape: "Escape", Tab: "Tab", Enter: "Enter", Backspace: "Backspace", Delete: "Delete",
      ArrowLeft: "ArrowLeft", ArrowRight: "ArrowRight", ArrowUp: "ArrowUp", ArrowDown: "ArrowDown",
      ShiftLeft: "Shift", ShiftRight: "Shift", ControlLeft: "Control", ControlRight: "Control",
      AltLeft: "Alt", AltRight: "Alt", MetaLeft: "Meta", MetaRight: "Meta", SuperLeft: "Super",
      SuperRight: "Super", OSLeft: "OS", OSRight: "OS",
      Home: "Home", End: "End", PageUp: "PageUp", PageDown: "PageDown", Insert: "Insert",
      Minus: "-", Equal: "=", BracketLeft: "[", BracketRight: "]", Backslash: "\\",
      Semicolon: ";", Quote: "'", Comma: ",", Period: ".", Slash: "/", Backquote: "`"
    };
    return named[code] || code;
  }

  /* ------------------------------------------------------------ coordinates */

  function pos(event) {
    const rect = els.screen.getBoundingClientRect();
    if (!rect.width || !rect.height) return null;
    const width = els.screen.videoWidth || FALLBACK_SIZE.w;
    const height = els.screen.videoHeight || FALLBACK_SIZE.h;
    // object-fit: contain — the picture is letterboxed inside the element box.
    const scale = Math.min(rect.width / width, rect.height / height);
    const contentW = width * scale;
    const contentH = height * scale;
    const offsetX = (rect.width - contentW) / 2;
    const offsetY = (rect.height - contentH) / 2;
    const rawX = (event.clientX - rect.left - offsetX) / scale;
    const rawY = (event.clientY - rect.top - offsetY) / scale;
    const inside = rawX >= -2 && rawY >= -2 && rawX <= width + 2 && rawY <= height + 2;
    return {
      x: clamp(Math.round(rawX), 0, width - 1),
      y: clamp(Math.round(rawY), 0, height - 1),
      inside
    };
  }

  // Absolute pointer motion is coalesced into one message per animation frame.
  // Time-based throttling (the old 16 ms gate) adds a whole frame of latency at
  // 60 Hz for no benefit: rAF already caps the rate, and keeping only the newest
  // position means the host sees the cursor where the user actually is.
  const absoluteMove = { pending: null, frame: 0 };

  function cancelQueuedMove() {
    absoluteMove.pending = null;
    if (absoluteMove.frame) {
      cancelAnimationFrame(absoluteMove.frame);
      absoluteMove.frame = 0;
    }
  }

  function moveTo(point) {
    cancelQueuedMove();
    S.pos = { x: point.x, y: point.y };
    S.hasPos = true;
    // During a drag, pointer motion must stay ordered between the button down
    // and up events. The lossy motion channel can otherwise reorder a drag or
    // arrive after the release, cancelling a real click at the target.
    const message = { type: "move", x: point.x, y: point.y };
    if (S.buttons.size) sendControl(message);
    else sendMotion(message);
  }

  function queueMove(point) {
    absoluteMove.pending = point;
    if (!absoluteMove.frame) {
      absoluteMove.frame = requestAnimationFrame(() => {
        absoluteMove.frame = 0;
        const pending = absoluteMove.pending;
        absoluteMove.pending = null;
        if (!pending || S.locked) return;
        moveTo(pending);
      });
    }
  }

  /* ------------------------------------------------------------ pointer I/O */

  const touch = {
    pointers: new Map(),
    mode: null,
    startX: 0, startY: 0, startAt: 0,
    moved: false,
    longPress: 0,
    scrollX: 0, scrollY: 0
  };
  let wheelAccum = { x: 0, y: 0 };
  let wheelFrame = 0;

  function flushWheel() {
    wheelFrame = 0;
    if (!wheelAccum.x && !wheelAccum.y) return;
    sendControl({
      type: "wheel",
      x: clamp(Math.round(wheelAccum.x), -250, 250),
      y: clamp(Math.round(wheelAccum.y), -250, 250)
    });
    wheelAccum = { x: 0, y: 0 };
  }

  function queueWheel(dx, dy) {
    wheelAccum.x += dx;
    wheelAccum.y += dy;
    if (!wheelFrame) wheelFrame = requestAnimationFrame(flushWheel);
  }

  function touchCentroid() {
    let x = 0, y = 0, n = 0;
    for (const pointer of touch.pointers.values()) { x += pointer.x; y += pointer.y; n += 1; }
    return n ? { x: x / n, y: y / n } : null;
  }

  function cancelLongPress() {
    clearTimeout(touch.longPress);
    touch.longPress = 0;
  }

  function onPointerDown(event) {
    pointerHealth.down += 1;
    if (S.locked) {
      // Locked: motion arrives as relative deltas, so a click only carries the
      // button state and must not re-anchor the host pointer.
      event.preventDefault();
      if (event.button >= 0 && event.button <= 2) {
        S.buttons.add(event.button);
        sendButton({ type: "button", button: event.button, down: true });
      }
      return;
    }
    if (event.target !== els.screen) return;
    els.screen.focus({ preventScroll: true });

    if (event.pointerType === "touch") {
      event.preventDefault();
      touch.pointers.set(event.pointerId, { x: event.clientX, y: event.clientY });
      try { els.screen.setPointerCapture(event.pointerId); } catch { /* capture unsupported */ }

      if (touch.pointers.size === 1) {
        const point = pos(event);
        if (!point) return;
        touch.mode = "tap";
        touch.startX = event.clientX;
        touch.startY = event.clientY;
        touch.startAt = performance.now();
        touch.moved = false;
        moveTo(point);
        cancelLongPress();
        touch.longPress = setTimeout(() => {
          if (touch.pointers.size !== 1 || touch.moved) return;
          // long press → secondary click, the touch equivalent of a right click
          sendButton({ type: "button", button: 2, down: true, x: S.pos.x, y: S.pos.y });
          sendButton({ type: "button", button: 2, down: false, x: S.pos.x, y: S.pos.y });
          touch.mode = "longpress";
          if (navigator.vibrate) navigator.vibrate(12);
        }, 520);
      } else if (touch.pointers.size === 2) {
        cancelLongPress();
        touch.mode = "scroll";
        touch.moved = true;
        const centroid = touchCentroid();
        touch.scrollX = centroid ? centroid.x : 0;
        touch.scrollY = centroid ? centroid.y : 0;
      }
      return;
    }

    event.preventDefault();
    const point = pos(event);
    if (!point) return;
    moveTo(point);
    if (event.button >= 0 && event.button <= 2) {
      S.buttons.add(event.button);
      sendControl({ type: "button", button: event.button, down: true, x: point.x, y: point.y });
    }
    try { els.screen.setPointerCapture(event.pointerId); } catch { /* capture unsupported */ }
  }

  function onPointerMove(event) {
    // Pointer lock delivers relative motion through mousemove; the absolute
    // path below would send frozen clientX/clientY and fight the host pointer.
    if (S.locked) return;
    if (event.pointerType === "touch") {
      if (!touch.pointers.has(event.pointerId)) return;
      touch.pointers.set(event.pointerId, { x: event.clientX, y: event.clientY });

      if (touch.pointers.size >= 2) {
        const centroid = touchCentroid();
        if (!centroid) return;
        if (touch.mode === "scroll") {
          queueWheel(-(centroid.x - touch.scrollX) * 1.6, -(centroid.y - touch.scrollY) * 1.6);
        }
        touch.scrollX = centroid.x;
        touch.scrollY = centroid.y;
        return;
      }

      const distance = Math.hypot(event.clientX - touch.startX, event.clientY - touch.startY);
      if (distance > 8) {
        touch.moved = true;
        cancelLongPress();
      }
      if (touch.mode === "longpress" || touch.mode === "scroll") return;
      const point = pos(event);
      if (point) queueMove(point);
      return;
    }

    if (S.buttons.size === 0 && event.target !== els.screen) return;
    const point = pos(event);
    if (point) queueMove(point);
  }

  function onPointerUp(event) {
    if (S.locked || S.buttons.has(event.button) || touch.pointers.has(event.pointerId)) {
      pointerHealth.up += 1;
    }
    if (S.locked) {
      if (S.buttons.has(event.button)) {
        S.buttons.delete(event.button);
        sendControl({ type: "button", button: event.button, down: false });
      }
      return;
    }
    if (event.pointerType === "touch") {
      if (!touch.pointers.has(event.pointerId)) return;
      touch.pointers.delete(event.pointerId);
      if (touch.pointers.size === 0) {
        cancelLongPress();
        if (touch.mode === "tap" && !touch.moved && performance.now() - touch.startAt < 480) {
          const point = pos(event);
          if (point) {
            moveTo(point);
            sendControl({ type: "button", button: 0, down: true, x: point.x, y: point.y });
            sendControl({ type: "button", button: 0, down: false, x: point.x, y: point.y });
          }
        }
        touch.mode = null;
        touch.scrollX = touch.scrollY = 0;
      } else if (touch.pointers.size === 1 && touch.mode === "scroll") {
        // one finger left: stop scrolling instead of jumping the pointer
        touch.mode = "tap";
        const left = touchCentroid();
        if (left) { touch.startX = left.x; touch.startY = left.y; }
        touch.moved = true;
      }
      try { els.screen.releasePointerCapture(event.pointerId); } catch { /* ignore */ }
      return;
    }

    if (S.buttons.has(event.button)) {
      S.buttons.delete(event.button);
      const point = pos(event);
      sendControl({
        type: "button", button: event.button, down: false,
        x: point ? point.x : S.pos.x, y: point ? point.y : S.pos.y
      });
    }
    try { els.screen.releasePointerCapture(event.pointerId); } catch { /* ignore */ }
  }

  function onPointerCancel(event) {
    pointerHealth.cancels += 1;
    if (event.pointerType === "touch") {
      touch.pointers.delete(event.pointerId);
      if (touch.pointers.size === 0) { cancelLongPress(); touch.mode = null; }
    }
    for (const button of [...S.buttons]) {
      sendControl(S.locked
        ? { type: "button", button, down: false }
        : { type: "button", button, down: false, x: S.pos.x, y: S.pos.y });
    }
    S.buttons.clear();
  }

  /* --------------------------------------------------------- physical keys */

  const PASSTHROUGH_CODES = new Set(["F5", "F11", "F12"]);
  const PASSTHROUGH_CTRL = new Set(["KeyR", "KeyW", "KeyT", "KeyN", "KeyQ", "Tab"]);

  function isTypingTarget(target) {
    if (!target) return false;
    const tag = target.tagName;
    return tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT" || tag === "BUTTON" || target.isContentEditable === true;
  }

  function shouldBlockBrowser(event) {
    if (PASSTHROUGH_CODES.has(event.code)) return false;
    if ((event.ctrlKey || event.metaKey) && PASSTHROUGH_CTRL.has(event.code)) return false;
    if (event.ctrlKey && event.shiftKey && ["KeyI", "KeyJ", "KeyC"].includes(event.code)) return false;
    return true;
  }

  // ChromeOS reports the Search key as ("OSLeft", "OS") on most builds,
  // ("MetaLeft", "Meta") on others, and a few shells drop `code` entirely.
  // Normalise so the host always receives a Meta-mapped key code.
  const SEARCH_KEY_NAMES = new Set(["OS", "Meta", "Super", "Win"]);

  function codeFor(event) {
    const code = event.code || "";
    if (code) return code;
    if (SEARCH_KEY_NAMES.has(event.key)) return "OSLeft";
    return "";
  }

  function remoteOwnsFocus() {
    return document.activeElement === els.screen || document.pointerLockElement === els.screen;
  }

  function onPhysicalKey(event) {
    // Full lock mode: Esc is the only way out and never reaches the host.
    if (event.key === "Escape") {
      if (S.locked) {
        if (event.type === "keydown") {
          S.escSwallowed = true;
          exitLockMode();
        } else {
          S.escSwallowed = false;
        }
        event.preventDefault();
        return;
      }
      if (S.escSwallowed && event.type === "keyup") {
        S.escSwallowed = false;
        event.preventDefault();
        return;
      }
    }

    // Esc always dismisses local overlays first, even from inside the sheet.
    if (event.key === "Escape" && event.type === "keydown") {
      if (document.body.dataset.fs === "pseudo") {
        setPseudoFullscreen(false);
        event.preventDefault();
        return;
      }
      if (!els.sheet.hidden) {
        setSheetOpen(false);
        event.preventDefault();
        return;
      }
    }

    // Typing into the local panel or any local control never reaches the host.
    if (els.sheet.contains(event.target) || isTypingTarget(event.target)) return;

    // Physical keys are forwarded only while the remote canvas owns focus.
    if (!remoteOwnsFocus()) return;

    if (!canSend()) {
      if (event.type === "keydown" && !event.repeat) {
        flashFooter(els.control.checked ? "Control channel not open yet" : "Remote input is switched off");
      }
      return;
    }

    if (shouldBlockBrowser(event)) event.preventDefault();
    const down = event.type === "keydown";
    const code = codeFor(event);
    const key = event.key;
    const locked = (name, fallback) => (typeof event.getModifierState === "function"
      ? event.getModifierState(name)
      : fallback);
    const extra = {
      repeat: event.repeat,
      ctrlKey: locked("Control", event.ctrlKey),
      altKey: locked("Alt", event.altKey),
      shiftKey: locked("Shift", event.shiftKey),
      metaKey: locked("Meta", event.metaKey)
    };

    if (down && event.repeat) {
      sendKey(code, key, true, extra);
      return;
    }
    if (down) S.pressed.add(code);
    else S.pressed.delete(code);
    sendKey(code, key, down, extra);
  }

  /* -------------------------------------------------------- on-screen keys */

  const SHIFT_SYMBOLS = {
    "!": "Digit1", "@": "Digit2", "#": "Digit3", "$": "Digit4", "%": "Digit5",
    "^": "Digit6", "&": "Digit7", "*": "Digit8", "(": "Digit9", ")": "Digit0",
    "_": "Minus", "+": "Equal", "{": "BracketLeft", "}": "BracketRight",
    "|": "Backslash", ":": "Semicolon", '"': "Quote", "<": "Comma", ">": "Period",
    "?": "Slash", "~": "Backquote"
  };
  const PLAIN_SYMBOLS = {
    " ": "Space", "\n": "Enter", "\t": "Tab", "-": "Minus", "=": "Equal",
    "[": "BracketLeft", "]": "BracketRight", "\\": "Backslash", ";": "Semicolon",
    "'": "Quote", ",": "Comma", ".": "Period", "/": "Slash", "`": "Backquote"
  };

  function charToStroke(ch) {
    if (/[a-z]/.test(ch)) return { code: "Key" + ch.toUpperCase(), key: ch, shift: false };
    if (/[A-Z]/.test(ch)) return { code: "Key" + ch, key: ch, shift: true };
    if (/[0-9]/.test(ch)) return { code: "Digit" + ch, key: ch, shift: false };
    if (SHIFT_SYMBOLS[ch]) return { code: SHIFT_SYMBOLS[ch], key: ch, shift: true };
    if (PLAIN_SYMBOLS[ch]) return { code: PLAIN_SYMBOLS[ch], key: ch, shift: false };
    return null;
  }

  function typeAsKeys(text) {
    let pending = "";
    for (const ch of text) {
      const stroke = charToStroke(ch);
      if (!stroke) { pending += ch; continue; }
      if (pending) { sendControl({ type: "text", text: pending }); pending = ""; }
      const extra = { ctrlKey: S.mods.ctrl, altKey: S.mods.alt, metaKey: S.mods.super, shiftKey: S.mods.shift || stroke.shift };
      sendKey(stroke.code, stroke.key, true, extra);
      // The down above just spent any latched modifier, so release plainly.
      sendKey(stroke.code, stroke.key, false);
    }
    if (pending) sendControl({ type: "text", text: pending });
  }

  function pressVirtualKey(code, key) {
    S.pressed.add(code);
    sendKey(code, key, true);
  }

  function releaseVirtualKey(code, key) {
    S.pressed.delete(code);
    sendKey(code, key, false);
  }

  function wireVirtualKey(button) {
    const code = button.dataset.code;
    const key = button.dataset.key != null ? button.dataset.key : code;
    let held = false;

    const down = (event) => {
      if (event.pointerType === "mouse" && event.button !== 0) return;
      event.preventDefault();
      if (held) return;
      held = true;
      button.classList.add("is-pressed");
      pressVirtualKey(code, key);
    };
    const up = () => {
      if (!held) return;
      held = false;
      button.classList.remove("is-pressed");
      releaseVirtualKey(code, key);
    };

    button.addEventListener("pointerdown", down);
    button.addEventListener("pointerup", up);
    button.addEventListener("pointerleave", up);
    button.addEventListener("pointercancel", up);
    button.addEventListener("click", (event) => {
      // Keyboard activation of the button (Enter/Space) has no pointer events.
      if (event.detail === 0) {
        pressVirtualKey(code, key);
        releaseVirtualKey(code, key);
      }
    });
  }

  document.querySelectorAll("[data-code]").forEach(wireVirtualKey);

  for (const button of document.querySelectorAll("[data-mod]")) {
    button.addEventListener("click", () => {
      const name = button.dataset.mod;
      if (!(name in S.mods)) return;
      S.mods[name] = !S.mods[name];
      syncModUi();
      const latched = latchedModifiers();
      flashFooter(latched.length
        ? "Modifier armed: " + latched.join(" + ") + " · applies to the next key"
        : "Modifiers cleared", 2600);
    });
  }

  els.control.addEventListener("change", () => {
    if (els.control.checked) {
      flashFooter("Remote input enabled");
    } else {
      releaseAll();
      flashFooter("Remote input disabled");
    }
  });

  /* ------------------------------------------------------ keyboard overlay */

  function setSheetOpen(open) {
    els.sheet.hidden = !open;
    document.body.classList.toggle("sheet-open", open);
    els.keyboard.setAttribute("aria-expanded", String(open));
    els.keybarKeys.setAttribute("aria-expanded", String(open));
    if (open) {
      if (document.body.dataset.sheet === "rail") setRailOpen(false);
      els.textInput.focus({ preventScroll: true });
    } else {
      els.screen.focus({ preventScroll: true });
    }
  }

  els.keyboard.addEventListener("click", () => setSheetOpen(els.sheet.hidden));
  els.fullKeyboard.addEventListener("click", () => setSheetOpen(els.sheet.hidden));
  els.keybarKeys.addEventListener("click", () => setSheetOpen(els.sheet.hidden));
  $("close-keyboard").addEventListener("click", () => setSheetOpen(false));

  els.textForm.addEventListener("submit", (event) => {
    event.preventDefault();
    const text = els.textInput.value;
    if (!text) return;
    if (S.kbMode === "keys") typeAsKeys(text);
    else sendControl({ type: "text", text: text.slice(0, 256) });
    els.textInput.value = "";
    els.textInput.focus({ preventScroll: true });
  });

  function setKeyboardMode(mode) {
    S.kbMode = mode === "keys" ? "keys" : "text";
    const keys = S.kbMode === "keys";
    els.kbModeText.classList.toggle("is-on", !keys);
    els.kbModeKeys.classList.toggle("is-on", keys);
    els.kbModeText.setAttribute("aria-pressed", String(!keys));
    els.kbModeKeys.setAttribute("aria-pressed", String(keys));
    els.textInput.placeholder = keys ? "Each character is sent as key down + up" : "Type text for the remote host";
    els.kbModeHint.textContent = keys
      ? "Sends characters one by one with key events, so shortcuts and games see real keys. Non-ASCII text still goes as text."
      : "Sends the whole line as typed text. Japanese and emoji work here.";
  }

  els.kbModeText.addEventListener("click", () => setKeyboardMode("text"));
  els.kbModeKeys.addEventListener("click", () => setKeyboardMode("keys"));

  /* --------------------------------------------------------------- rail UI */

  function setRailOpen(open) {
    document.body.dataset.sheet = open ? "rail" : "";
    els.railToggle.setAttribute("aria-expanded", String(open));
    if (open) setSheetOpen(false);
  }

  els.railToggle.addEventListener("click", () => setRailOpen(document.body.dataset.sheet !== "rail"));
  els.hudOpen.addEventListener("click", () => setRailOpen(true));
  els.railClose.addEventListener("click", () => setRailOpen(false));

  /* ----------------------------------------------------------- fullscreen */

  function setPseudoFullscreen(on) {
    document.body.dataset.fs = on ? "pseudo" : "";
    els.fullscreen.setAttribute("aria-pressed", String(on));
    els.fullscreen.setAttribute("aria-label", on ? "Exit fullscreen" : "Enter fullscreen");
    setFullscreenIcon(on);
    if (on) {
      els.screen.focus({ preventScroll: true });
      notice("Fullscreen API is unavailable here — showing an in-page fullscreen view.");
    } else {
      notice("");
    }
    syncViewport();
  }

  function setFullscreenIcon(on) {
    const use = els.fullscreen.querySelector("use");
    if (use) use.setAttribute("href", on ? "#i-collapse" : "#i-expand");
  }

  async function toggleFullscreen() {
    if (document.fullscreenElement) {
      try { await document.exitFullscreen(); } catch { /* ignore */ }
      return;
    }
    if (document.body.dataset.fs === "pseudo") {
      setPseudoFullscreen(false);
      return;
    }
    if (!els.stage.requestFullscreen) {
      setPseudoFullscreen(true);
      return;
    }
    try {
      await els.stage.requestFullscreen({ navigationUI: "hide" });
    } catch {
      setPseudoFullscreen(true);
    }
  }

  els.fullscreen.addEventListener("click", () => { toggleFullscreen(); });
  els.exitFullscreen.addEventListener("click", () => {
    if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
    else if (document.body.dataset.fs === "pseudo") setPseudoFullscreen(false);
  });

  document.addEventListener("fullscreenchange", () => {
    const on = !!document.fullscreenElement;
    els.fullscreen.setAttribute("aria-pressed", String(on));
    els.fullscreen.setAttribute("aria-label", on ? "Exit fullscreen" : "Enter fullscreen");
    setFullscreenIcon(on);
    if (on) {
      els.screen.focus({ preventScroll: true });
      armIdle();
    } else {
      els.stage.dataset.idle = "0";
    }
    syncViewport();
  });

  let idleTimer = 0;
  function armIdle() {
    clearTimeout(idleTimer);
    els.stage.dataset.idle = "0";
    idleTimer = setTimeout(() => {
      if (document.fullscreenElement || document.body.dataset.fs === "pseudo") {
        els.stage.dataset.idle = "1";
      }
    }, 2600);
  }
  els.stage.addEventListener("pointermove", () => {
    if (document.fullscreenElement || document.body.dataset.fs === "pseudo") armIdle();
  });

  /* -------------------------------------------------------------- lock mode */

  // Full lock mode hands the whole surface to the remote: pointer lock turns
  // mouse motion into relative deltas, keyboard lock keeps Ctrl+W / F11 / the
  // ChromeOS Search key for the remote instead of the browser. Esc is the single
  // documented way out, and it never reaches the host.
  const RELATIVE_CLAMP = 500; // the host clamps one relative message to ±500
  const relative = { x: 0, y: 0 };
  let relativeFrame = 0;
  let lockWatchdog = 0;

  const keyboardLockSupported = () =>
    !!(navigator.keyboard && typeof navigator.keyboard.lock === "function");

  function renderInputTelemetry() {
    if (els.inputMode) els.inputMode.textContent = S.locked ? "locked (relative)" : "absolute";
    if (els.kblock) els.kblock.textContent = S.keyboardLock;
  }

  function renderLockUi() {
    if (S.locked) document.body.dataset.lock = "on";
    else delete document.body.dataset.lock;
    for (const button of [els.lock, els.lockStage]) {
      if (!button) continue;
      button.setAttribute("aria-pressed", String(S.locked));
      button.setAttribute("aria-label", S.locked
        ? "Leave full lock mode (Esc)"
        : "Full lock mode: capture pointer and keyboard");
      button.title = S.locked ? "Locked · Esc to exit" : "Full lock mode";
    }
    if (els.hudLock) els.hudLock.hidden = !S.locked;
    renderInputTelemetry();
  }

  function requestKeyboardLock() {
    if (!keyboardLockSupported()) {
      S.keyboardLock = "unsupported";
      renderInputTelemetry();
      return;
    }
    let attempt = null;
    try {
      attempt = navigator.keyboard.lock();
    } catch {
      S.keyboardLock = "off";
      renderInputTelemetry();
      return;
    }
    S.keyboardLock = "on";
    if (attempt && typeof attempt.catch === "function") {
      attempt.catch(() => {
        S.keyboardLock = "off";
        renderInputTelemetry();
      });
    }
    renderInputTelemetry();
  }

  function releaseKeyboardLock() {
    S.keyboardLock = keyboardLockSupported() ? "off" : "unsupported";
    try {
      if (navigator.keyboard && typeof navigator.keyboard.unlock === "function") navigator.keyboard.unlock();
    } catch { /* unlock is best effort */ }
    renderInputTelemetry();
  }

  function flushRelative() {
    relativeFrame = 0;
    if (!S.locked) { relative.x = 0; relative.y = 0; return; }
    if (!relative.x && !relative.y) return;
    const dx = clamp(Math.round(relative.x), -RELATIVE_CLAMP, RELATIVE_CLAMP);
    const dy = clamp(Math.round(relative.y), -RELATIVE_CLAMP, RELATIVE_CLAMP);
    relative.x -= dx;
    relative.y -= dy;
    if (!sendMotion({ type: "move", relative: true, dx, dy })) {
      relative.x = 0;
      relative.y = 0;
      return;
    }
    if (relative.x || relative.y) relativeFrame = requestAnimationFrame(flushRelative);
  }

  function queueRelative(dx, dy) {
    if (!S.locked || (!dx && !dy)) return;
    const limit = RELATIVE_CLAMP * 4;
    relative.x = clamp(relative.x + dx, -limit, limit);
    relative.y = clamp(relative.y + dy, -limit, limit);
    if (!relativeFrame) relativeFrame = requestAnimationFrame(flushRelative);
  }

  function applyLockState(locked) {
    const was = S.locked;
    S.locked = !!locked;
    clearTimeout(lockWatchdog);
    lockWatchdog = 0;
    cancelQueuedMove();
    if (!S.locked) {
      releaseKeyboardLock();
      relative.x = 0;
      relative.y = 0;
      if (relativeFrame) { cancelAnimationFrame(relativeFrame); relativeFrame = 0; }
      releaseAll();
      els.screen.focus({ preventScroll: true });
    }
    renderLockUi();
    if (S.locked && !was) flashFooter("Full lock mode · relative pointer · Esc to exit", 3200);
    else if (!S.locked && was) flashFooter("Full lock mode ended · absolute input", 2600);
  }

  function enterLockMode() {
    if (S.locked) { exitLockMode(); return; }
    if (!els.screen.requestPointerLock) {
      notice("Pointer lock is unavailable in this browser — input stays absolute.", 4400);
      return;
    }
    // A local text panel would keep the keyboard for itself.
    if (!els.sheet.hidden) setSheetOpen(false);
    // Both locks need the activation from this click, so ask for them in the
    // same task: no await may spend the user gesture in between. The plain call
    // is used deliberately — the `unadjustedMovement` dictionary is rejected on
    // several platforms and the failed request also raised pointerlockerror.
    els.screen.focus({ preventScroll: true });
    requestKeyboardLock();
    try {
      const attempt = els.screen.requestPointerLock();
      if (attempt && typeof attempt.catch === "function") {
        attempt.catch(() => { releaseKeyboardLock(); });
      }
    } catch {
      releaseKeyboardLock();
      notice("The browser refused pointer lock — input stays absolute.", 4400);
      return;
    }
    // Safety net: keyboard lock must never outlive a pointer lock that never
    // arrived, or the browser keys would be captured with no way back.
    clearTimeout(lockWatchdog);
    lockWatchdog = setTimeout(() => {
      lockWatchdog = 0;
      if (!S.locked) releaseKeyboardLock();
    }, 1500);
  }

  function exitLockMode() {
    if (document.pointerLockElement === els.screen) {
      try { document.exitPointerLock(); } catch { /* already gone */ }
    }
    // pointerlockchange lands a frame later; clean up now so a dropped event can
    // never leave the page half-locked.
    applyLockState(false);
  }

  document.addEventListener("pointerlockchange", () => {
    applyLockState(document.pointerLockElement === els.screen);
  });

  document.addEventListener("pointerlockerror", () => {
    releaseKeyboardLock();
    applyLockState(false);
    notice("The browser refused pointer lock — input stays absolute.", 4400);
  });

  document.addEventListener("mousemove", (event) => {
    if (!S.locked) return;
    if (document.pointerLockElement !== els.screen) return;
    queueRelative(event.movementX || 0, event.movementY || 0);
  }, { passive: true });

  for (const button of [els.lock, els.lockStage]) {
    if (button) button.addEventListener("click", () => { enterLockMode(); });
  }

  // Inspection hook: pointer lock is a browser permission, so the E2E harness
  // can still drive relative input and read the mode where a lock is refused.
  window.__swayrtcLockHook = {
    state: () => ({
      locked: S.locked,
      pointerLock: document.pointerLockElement === els.screen,
      keyboardLock: S.keyboardLock,
      inputMode: S.locked ? "locked(relative)" : "absolute"
    }),
    enter: () => enterLockMode(),
    exit: () => exitLockMode(),
    force: (on) => applyLockState(on !== false),
    relative: (dx, dy) => queueRelative(dx, dy)
  };

  /* --------------------------------------------------------- video playback */

  function startVideo() {
    const attempt = els.screen.play();
    if (attempt && typeof attempt.then === "function") {
      attempt.then(() => hideOverlay()).catch(() => {});
    } else {
      hideOverlay();
    }
  }

  function playVideo() {
    const attempt = els.screen.play();
    if (!attempt || typeof attempt.catch !== "function") return;
    attempt.catch(() => {
      // Some browsers have already started the muted stream internally; only
      // show the gesture prompt when playback is genuinely blocked.
      if (els.screen.readyState >= 2 && !els.screen.paused) { hideOverlay(); return; }
      showOverlay({
        headline: "Video needs a gesture",
        detail: "This browser blocked autoplay for the remote stream.",
        advice: "The stream is muted; tap or click Start video to begin.",
        action: { label: "Start video", run: startVideo }
      });
    });
  }

  els.screen.addEventListener("playing", () => hideOverlay());
  els.screen.addEventListener("click", () => {
    els.screen.focus({ preventScroll: true });
    if (els.screen.paused) els.screen.play().catch(() => {});
  });
  els.screen.addEventListener("contextmenu", (event) => event.preventDefault());
  els.screen.addEventListener("wheel", (event) => {
    event.preventDefault();
    const unit = event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? 400 : 1;
    sendControl({
      type: "wheel",
      x: clamp(Math.round(event.deltaX * unit), -250, 250),
      y: clamp(Math.round(event.deltaY * unit), -250, 250)
    });
  }, { passive: false });

  els.screen.addEventListener("pointerdown", onPointerDown);
  window.addEventListener("pointermove", onPointerMove, { passive: true });
  window.addEventListener("pointerup", onPointerUp);
  window.addEventListener("pointercancel", onPointerCancel);
  document.addEventListener("keydown", onPhysicalKey, true);
  document.addEventListener("keyup", onPhysicalKey, true);

  els.screen.addEventListener("blur", releaseAll);
  window.addEventListener("blur", releaseAll);
  document.addEventListener("visibilitychange", onVisibilityChange);
  window.addEventListener("pagehide", releaseAll);

  els.reconnect.addEventListener("click", () => connect({ manual: true }));
  els.reconnectTop.addEventListener("click", () => connect({ manual: true }));
  els.logout.addEventListener("click", async () => {
    S.stopping = true;
    try { await fetch("/api/logout", { method: "POST" }); } catch { /* go home anyway */ }
    location.href = "/";
  });

  /* -------------------------------------------------------------- viewport */

  function syncViewport() {
    const viewport = window.visualViewport;
    const height = viewport ? viewport.height : window.innerHeight;
    document.documentElement.style.setProperty("--vvh", Math.round(height) + "px");
  }

  if (window.visualViewport) {
    window.visualViewport.addEventListener("resize", syncViewport);
    window.visualViewport.addEventListener("scroll", syncViewport);
  }
  window.addEventListener("resize", () => { syncViewport(); layoutWebCodecs(); });
  window.addEventListener("orientationchange", () => setTimeout(syncViewport, 250));

  /* ------------------------------------------------------------------ boot */

  S.mobile = window.matchMedia("(max-width: 820px), (pointer: coarse)").matches;
  try {
    const stored = sessionStorage.getItem(POLICY_KEY);
    if (stored && POLICIES.includes(stored)) S.icePolicy = stored;
  } catch { /* storage blocked */ }
  applyPolicyUi();
  // WebCodecs paints ~6 ms earlier than the media element, so it is the default
  // wherever the encoded-transform API exists; the toggle still forces either.
  const canWebCodecs = typeof RTCRtpScriptTransform !== "undefined" && typeof VideoDecoder !== "undefined";
  try { S.renderMode = sessionStorage.getItem(RENDER_KEY) || (canWebCodecs ? "webcodecs" : "element"); } catch { S.renderMode = canWebCodecs ? "webcodecs" : "element"; }
  function syncRenderUi() {
    for (const button of document.querySelectorAll("[data-render]")) {
      const on = button.dataset.render === S.renderMode;
      button.classList.toggle("is-on", on);
      button.setAttribute("aria-pressed", String(on));
    }
    renderNote(S.renderMode === "element"
      ? "Video element renderer. Switch to WebCodecs to decode the RTP stream directly onto a canvas."
      : "WebCodecs renderer: decoding straight from the RTP stream, painting a canvas over the video element.");
  }
  function setRenderMode(mode, { reconnect = true } = {}) {
    if (mode !== "element" && mode !== "webcodecs") return;
    if (S.renderMode === mode) return;
    S.renderMode = mode;
    try { sessionStorage.setItem(RENDER_KEY, mode); } catch { /* storage blocked */ }
    syncRenderUi();
    if (reconnect && S.ws) connect({ manual: true });
  }
  for (const button of document.querySelectorAll("[data-render]")) {
    button.addEventListener("click", () => setRenderMode(button.dataset.render));
  }
  syncRenderUi();
  window.__swayrtcRenderer = { mode: () => S.renderMode, set: (m) => setRenderMode(m), stats: () => (S.wc ? { drawn: S.wc.drawn, decodeDrawMs: S.wc.decodeDrawMs } : null) };

  for (const button of document.querySelectorAll("[data-policy]")) {
    button.addEventListener("click", () => setIcePolicy(button.dataset.policy));
  }
  loadIceConfig();
  setKeyboardMode("text");
  syncViewport();
  resetRows();
  S.keyboardLock = keyboardLockSupported() ? "off" : "unsupported";
  renderLockUi();
  setChip(els.chipSig, "idle");
  setChip(els.chipIce, "idle");
  setChip(els.chipDc, "idle");
  setStatus("Connecting", "wait");
  connect();
})();
