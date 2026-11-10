// SwayRTC encoded-transform worker.
//
// The viewer installs this worker as an RTCRtpScriptTransform on the video
// receiver. Chrome hands the encoded RTP stream to the worker (this is the only
// place where the encoded frames can be taken over - createEncodedStreams()
// throws "Too late" once the receiver has started), the worker decodes with
// WebCodecs and posts the decoded VideoFrame back to the page, which paints it
// on a canvas. The element still receives the same frames because the worker
// forwards them downstream.

let decoder = null;
let port = null;
let decodeErrors = 0;
const stats = { chunks: 0, decoded: 0, transforms: 0, hasPort: false, hasPending: false, started: 0, err: null, state: "init" };

function setupDecoder() {
  decoder = new VideoDecoder({
    output: (frame) => {
      stats.decoded += 1;
      if (!port) {
        frame.close();
        return;
      }
      // The frame is transferable, so the page can draw it without a copy.
      port.postMessage({ type: "frame", frame, timestamp: frame.timestamp }, [frame]);
    },
    error: (err) => {
      stats.err = String(err && err.message || err).slice(0, 120);
      decodeErrors += 1;
      if (port && decodeErrors <= 3) port.postMessage({ type: "error", message: String(err && err.message || err).slice(0, 120) });
    },
  });
  // Constrained baseline, level 4.2 covers 1080p60. The stream is Annex-B, which
  // is what WebRTC delivers and what the decoder must be told to expect.
  const fmt = "annexb";
  decoder.configure({ codec: "avc1.42E02A", optimizeForLatency: true, avc: { format: fmt } });
}

// The transform event and the port from the page race each other: hold the
// streams until the port is here so no decoded frame is thrown away.
let pending = null;

function startIfReady() {
  stats.hasPending = !!pending;
  stats.hasPort = !!port;
  if (!pending || !port) return;
  stats.started += 1;
  const transformer = pending;
  pending = null;
  const writer = transformer.writable.getWriter();
  const reader = transformer.readable.getReader();
  setupDecoder();
  (async () => {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      if (!value) continue;
      stats.chunks += 1;
      try {
        // Copy the payload first: writing the frame downstream detaches its data.
        const payload = new Uint8Array(value.data);
        if (stats.chunks === 1) {
          stats.firstType = value.type;
          stats.firstTs = value.timestamp;
        }
        stats.lastTs = value.timestamp;
        if (decoder) {
          try {
            decoder.decode(new EncodedVideoChunk({
              type: value.type === "key" ? "key" : "delta",
              timestamp: value.timestamp,
              data: payload,
            }));
            stats.state = decoder.state;
          } catch (err) {
            stats.err = String(err && err.message || err).slice(0, 120);
          }
        }
        // Forward so the media element keeps decoding and stays the surface that
        // owns size, focus and pointer lock.
        try { await writer.write(value); } catch { /* downstream gone */ }
      } catch (err) {
        stats.err = String(err && err.message || err).slice(0, 120);
      }
    }
  })().catch((err) => {
    if (port) port.postMessage({ type: "error", message: String(err && err.message || err).slice(0, 120) });
  });
}

self.onmessage = (event) => {
  const data = event.data || {};
  if (data.query) {
    stats.hasPending = !!pending;
    stats.hasPort = !!port;
    if (port) port.postMessage({ type: "stats", stats });
    else if (event.ports && event.ports[0]) event.ports[0].postMessage({ type: "stats", stats });
    return;
  }
  if (data.port) {
    port = data.port;
    stats.hasPort = true;
    startIfReady();
  }
  if (data.stop) {
    try { decoder && decoder.close(); } catch { /* already closed */ }
    decoder = null;
  }
};

self.onrtctransform = (event) => {
  stats.transforms += 1;
  pending = event.transformer;
  stats.hasPending = !!pending;
  if (port) port.postMessage({ type: "diag", what: "transform" });
  startIfReady();
};
