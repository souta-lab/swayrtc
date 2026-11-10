import { readFileSync } from "node:fs";
import { randomBytes, timingSafeEqual } from "node:crypto";
import { userInfo } from "node:os";
import { join } from "node:path";

/*
 * SwayRTC signaling server
 * ------------------------
 * - Authentication: PAM via `pamtester login <ACCOUNT> authenticate`, password on stdin.
 *   pam_unix authenticates only the account the process itself runs as, so the
 *   service MUST run as ACCOUNT. Never run as root.
 * - Sessions: 32-byte crypto-random key, HttpOnly/SameSite=Strict cookie, 12h TTL.
 * - Never logs, returns, or forwards the password, and never exposes pamtester output.
 */

type Role = "host" | "viewer";
type Peer = { role: Role; session?: string };

/** Structural subset of Bun's server object we depend on (avoids `any`). */
type ServerRef = {
  requestIP(request: Request): { address: string } | null;
  upgrade(request: Request, options: { data: Peer }): boolean;
};

const ROOT = import.meta.dir;
const WEB_ROOT = join(ROOT, "web");
const PORT = Number(process.env.PORT || 8777);
const BIND = process.env.BIND || "127.0.0.1";
const ACCOUNT = (process.env.SWAYRTC_ACCOUNT || userInfo().username).trim();
const DEBUG = process.env.SWAYRTC_DEBUG === "1";

const SESSION_TTL_MS = 12 * 60 * 60 * 1000;
const SESSION_MAX = 64;
const SWEEP_INTERVAL_MS = 10 * 60 * 1000;
const LOGIN_WINDOW_MS = 5 * 60 * 1000;
const LOGIN_LOCK_MS = 5 * 60 * 1000;
const LOGIN_MAX_FAILURES = 5;
const LOGIN_MIN_RESPONSE_MS = 300;
const MAX_CONCURRENT_AUTH = 3;
const PAM_TIMEOUT_MS = 10_000;
const MAX_PASSWORD_LEN = 256;
const MAX_USERNAME_LEN = 128;
const MAX_LOGIN_BODY = 4 * 1024;
const MAX_TRACKED_CLIENTS = 4096;

const log = (...args: unknown[]) => { if (DEBUG) console.error("[signal]", ...args); };

/* ------------------------------------------------------------------ */
/* state                                                              */
/* ------------------------------------------------------------------ */

/** session key (64 hex chars) -> expiry timestamp (ms). */
const sessions = new Map<string, number>();
/** rate-limit bucket -> recent failures / lockout. */
const failures = new Map<string, { count: number; last: number; until: number }>();
/** WebSocket slots; a new connection replaces the previous one. */
let host: ServerWebSocket<Peer> | undefined;
let viewer: ServerWebSocket<Peer> | undefined;
/** Number of pamtester processes currently running (bounded work). */
let activeAuths = 0;

const hostToken = (() => {
  try {
    const token = readFileSync(join(ROOT, ".secrets/host-token"), "utf8").trim();
    if (!token) throw new Error("empty host token");
    return token;
  } catch {
    console.error("[fatal] cannot read .secrets/host-token; refusing to start");
    process.exit(1);
  }
})();

/* ------------------------------------------------------------------ */
/* small helpers                                                      */
/* ------------------------------------------------------------------ */

const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

const isHttps = (request: Request): boolean => {
  const forwarded = request.headers.get("x-forwarded-proto");
  if (forwarded && forwarded.split(",")[0]!.trim().toLowerCase() === "https") return true;
  try { return new URL(request.url).protocol === "https:"; } catch { return false; }
};

const securityHeaders = (request: Request): Record<string, string> => {
  const headers: Record<string, string> = {
    "cache-control": "no-store",
    "x-content-type-options": "nosniff",
    "referrer-policy": "no-referrer",
    "x-frame-options": "DENY",
    "content-security-policy": "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self' wss: ws:; img-src 'self' data:; media-src 'self' blob:; base-uri 'none'; object-src 'none'; frame-ancestors 'none'",
  };
  // Cloudflare terminates TLS; HSTS is only meaningful (and only sent) over HTTPS.
  if (isHttps(request)) headers["strict-transport-security"] = "max-age=31536000";
  return headers;
};

const json = (request: Request, value: unknown, status = 200, extra: Record<string, string> = {}): Response =>
  new Response(JSON.stringify(value), {
    status,
    headers: { ...securityHeaders(request), "content-type": "application/json; charset=utf-8", ...extra },
  });

const text = (request: Request, value: string, status = 200, extra: Record<string, string> = {}): Response =>
  new Response(value, { status, headers: { ...securityHeaders(request), "content-type": "text/plain; charset=utf-8", ...extra } });

const mimeFor = (name: string): string =>
  name.endsWith(".html") ? "text/html; charset=utf-8"
    : name.endsWith(".css") ? "text/css; charset=utf-8"
      : "text/javascript; charset=utf-8";

/* ------------------------------------------------------------------ */
/* client identity / rate limiting                                    */
/* ------------------------------------------------------------------ */

const HEXTET = /^[0-9a-f]{1,4}$/;

/** Expand an IPv6 address into 8 hextets, or null when it is not valid IPv6. */
const expandIpv6 = (ip: string): string[] | null => {
  const halves = ip.split("::");
  if (halves.length > 2) return null;
  const left = halves[0] ? halves[0].split(":") : [];
  const right = halves.length === 2 && halves[1] ? halves[1].split(":") : [];
  if (halves.length === 1) return left.length === 8 && left.every((h) => HEXTET.test(h)) ? left : null;
  const missing = 8 - left.length - right.length;
  if (missing < 0) return null;
  const parts = [...left, ...new Array<string>(missing).fill("0"), ...right];
  return parts.every((h) => HEXTET.test(h)) ? parts : null;
};

/** Normalize an address for bucket keys: lowercase, no zone, IPv4-mapped -> IPv4. */
const normalizeIp = (raw: string): string => {
  let ip = (raw || "").trim().toLowerCase();
  const zone = ip.indexOf("%");
  if (zone >= 0) ip = ip.slice(0, zone);
  if (ip.startsWith("[") && ip.endsWith("]")) ip = ip.slice(1, -1);
  if (ip.startsWith("::ffff:") && ip.includes(".")) ip = ip.slice("::ffff:".length);
  if (ip === "0:0:0:0:0:0:0:1") ip = "::1";
  return ip;
};

const isLoopback = (ip: string): boolean => ip === "::1" || ip.startsWith("127.") || ip === "0.0.0.0" || ip === "";

const isIpLike = (value: string): boolean => value.length > 0 && value.length <= 64 && /^[0-9a-f:.]+$/.test(value);

/**
 * Bucket key for rate limiting. IPv6 clients get grouped by /64 so that a
 * single host cannot rotate through its prefix to bypass the limiter.
 */
const rateKey = (ip: string): string => {
  if (!ip.includes(":")) return ip || "unknown";
  const hextets = expandIpv6(ip);
  if (!hextets) return ip;
  return hextets.slice(0, 4).map((h) => h.replace(/^0+(?=.)/, "")).join(":") + "::/64";
};

/**
 * Resolve the client address used for rate limiting.
 *
 * The public entry point is a Cloudflare Tunnel, whose local endpoint is
 * loopback; the edge overwrites CF-Connecting-IP, so it is trustworthy exactly
 * when the peer is loopback. A plain local connection carries no such header
 * and therefore falls back to the real socket address.
 */
const clientIp = (request: Request, server: ServerRef): string => {
  const peer = normalizeIp(server.requestIP(request)?.address ?? "");
  if (isLoopback(peer)) {
    const forwarded = normalizeIp(request.headers.get("cf-connecting-ip") ?? "");
    if (isIpLike(forwarded)) return rateKey(forwarded);
  }
  return rateKey(peer || "unknown");
};

/** Seconds until retry when the bucket is locked, otherwise 0. */
const lockedFor = (key: string, now: number): number => {
  const record = failures.get(key);
  if (!record) return 0;
  if (record.until > now) return Math.max(1, Math.ceil((record.until - now) / 1000));
  // Lockout served, or the failure window decayed: start counting from zero.
  if (record.until > 0 || now - record.last > LOGIN_WINDOW_MS) failures.delete(key);
  return 0;
};

const recordFailure = (key: string, now: number): void => {
  const previous = failures.get(key);
  const fresh = !previous || now - previous.last > LOGIN_WINDOW_MS;
  const count = (fresh ? 0 : previous.count) + 1;
  failures.set(key, {
    count,
    last: now,
    until: count >= LOGIN_MAX_FAILURES ? now + LOGIN_LOCK_MS : 0,
  });
  if (failures.size > MAX_TRACKED_CLIENTS) pruneFailures(now);
};

const pruneFailures = (now: number): void => {
  for (const [key, record] of failures) {
    if (record.until <= now && now - record.last > LOGIN_WINDOW_MS) failures.delete(key);
  }
  if (failures.size <= MAX_TRACKED_CLIENTS) return;
  const oldest = [...failures.entries()].sort((a, b) => a[1].last - b[1].last);
  for (const [key] of oldest.slice(0, oldest.length - MAX_TRACKED_CLIENTS)) failures.delete(key);
};

/* ------------------------------------------------------------------ */
/* sessions                                                           */
/* ------------------------------------------------------------------ */

const cookie = (request: Request): string | undefined => {
  const header = request.headers.get("cookie");
  if (!header) return undefined;
  for (const part of header.split(";")) {
    const eq = part.indexOf("=");
    if (eq < 0) continue;
    if (part.slice(0, eq).trim() !== "sr_session") continue;
    const value = part.slice(eq + 1).trim();
    if (/^[0-9a-f]{64}$/.test(value)) return value;
  }
  return undefined;
};

const sessionCookie = (request: Request, value: string, maxAgeSeconds: number): string =>
  `sr_session=${value}; Path=/; HttpOnly; SameSite=Strict; Max-Age=${maxAgeSeconds}` + (isHttps(request) ? "; Secure" : "");

const pruneSessions = (now: number): void => {
  for (const [key, expires] of sessions) if (expires <= now) sessions.delete(key);
  // Hard cap: drop the sessions that expire soonest (oldest logins).
  while (sessions.size >= SESSION_MAX) {
    let victim: string | undefined;
    let soonest = Number.POSITIVE_INFINITY;
    for (const [key, expires] of sessions) {
      if (expires < soonest) { soonest = expires; victim = key; }
    }
    if (!victim) break;
    sessions.delete(victim);
  }
};

const issueSession = (): string => {
  const now = Date.now();
  if (sessions.size >= SESSION_MAX) pruneSessions(now);
  const key = randomBytes(32).toString("hex");
  sessions.set(key, now + SESSION_TTL_MS);
  return key;
};

const authorized = (request: Request): { key: string; expires: number } | undefined => {
  const key = cookie(request);
  if (!key) return undefined;
  const expires = sessions.get(key);
  if (expires === undefined) return undefined;
  if (expires <= Date.now()) {
    sessions.delete(key);
    return undefined;
  }
  return { key, expires };
};

/* ------------------------------------------------------------------ */
/* CSRF / origin checks                                               */
/* ------------------------------------------------------------------ */

const normalizeHost = (value: string): string =>
  value.trim().toLowerCase().replace(/:(?:443|80)$/, "");

/**
 * Optional comma-separated allowlist for the public origin hostname(s), e.g.
 * SWAYRTC_ALLOWED_ORIGINS=swayrtc.example.com. Only needed when the reverse
 * proxy rewrites Host (cloudflared forwards the public Host by default).
 */
const EXTRA_ORIGIN_HOSTS = (process.env.SWAYRTC_ALLOWED_ORIGINS ?? "")
  .split(",")
  .map(normalizeHost)
  .filter(Boolean);

const allowedHosts = (request: Request): string[] => {
  const raw = [request.headers.get("x-forwarded-host"), request.headers.get("host")].filter(Boolean).join(",");
  const hosts = raw.split(",").map(normalizeHost).filter(Boolean);
  for (const extra of EXTRA_ORIGIN_HOSTS) if (!hosts.includes(extra)) hosts.push(extra);
  return hosts;
};

const matchesAllowedHost = (value: string, request: Request): boolean => {
  let parsed: URL;
  try { parsed = new URL(value); } catch { return false; }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") return false;
  const candidate = normalizeHost(parsed.host);
  return allowedHosts(request).includes(candidate);
};

/** Strict check for browser upgrades: Origin must be present and same-host. */
const originMatches = (request: Request): boolean => {
  const origin = request.headers.get("origin");
  return origin !== null && matchesAllowedHost(origin, request);
};

/**
 * CSRF guard for state-changing requests. Browsers always attach Origin to a
 * cross-site POST; we reject any Origin/Referer that is not our own host.
 * Header-less clients (curl, tests) cannot be victims of CSRF and pass.
 */
const sameOriginOk = (request: Request): boolean => {
  const origin = request.headers.get("origin");
  if (origin !== null) return matchesAllowedHost(origin, request);
  const referer = request.headers.get("referer");
  if (referer !== null) return matchesAllowedHost(referer, request);
  return true;
};

/* ------------------------------------------------------------------ */
/* PAM authentication                                                 */
/* ------------------------------------------------------------------ */

const equalizeLoginTiming = async (started: number): Promise<void> => {
  const target = LOGIN_MIN_RESPONSE_MS + Math.random() * 150;
  const elapsed = performance.now() - started;
  if (elapsed < target) await sleep(target - elapsed);
};

/**
 * Authenticate via PAM. Never leaks pamtester's output, and never leaves a
 * child process behind: the child is always awaited (reaped), and killed if it
 * exceeds the timeout.
 */
const pamAuthenticate = async (password: string): Promise<"ok" | "fail" | "busy"> => {
  if (activeAuths >= MAX_CONCURRENT_AUTH) return "busy";
  activeAuths++;
  let child: ReturnType<typeof Bun.spawn> | undefined;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let timedOut = false;
  try {
    child = Bun.spawn(["/usr/bin/pamtester", "login", ACCOUNT, "authenticate"], {
      stdin: "pipe",
      stdout: "ignore",
      stderr: "ignore",
    });
    try {
      child.stdin.write(password + "\n");
      child.stdin.end();
    } catch {
      return "fail";
    }
    timer = setTimeout(() => {
      timedOut = true;
      try { child!.kill("SIGKILL"); } catch { /* already gone */ }
    }, PAM_TIMEOUT_MS);
    const code = await child.exited;
    return !timedOut && code === 0 ? "ok" : "fail";
  } catch {
    return "fail";
  } finally {
    if (timer) clearTimeout(timer);
    if (child && child.exitCode === null) {
      try { child.kill("SIGKILL"); } catch { /* already gone */ }
      // Reap without blocking the response path.
      void child.exited.catch(() => {});
    }
    activeAuths--;
  }
};

/* ------------------------------------------------------------------ */
/* request handlers                                                   */
/* ------------------------------------------------------------------ */

const handleLogin = async (request: Request, server: ServerRef): Promise<Response> => {
  const started = performance.now();
  if (!sameOriginOk(request)) return json(request, { error: "Cross-site request rejected" }, 403);
  if (Number(request.headers.get("content-length") ?? 0) > MAX_LOGIN_BODY) {
    return json(request, { error: "Payload too large" }, 413);
  }

  const ip = clientIp(request, server);
  const retryAfter = lockedFor(ip, Date.now());
  if (retryAfter > 0) {
    return json(request, { error: "Too many attempts" }, 429, {
      "retry-after": String(retryAfter),
      "cache-control": "no-store",
    });
  }

  let username = "";
  let password = "";
  try {
    const body = (await request.json()) as { username?: unknown; password?: unknown } | null;
    if (body && typeof body === "object") {
      if (typeof body.username === "string") username = body.username.trim().slice(0, MAX_USERNAME_LEN);
      if (typeof body.password === "string") password = body.password;
    }
  } catch {
    // Malformed payload: treat as a failed attempt without invoking PAM.
  }

  let outcome: "ok" | "fail" | "busy" = "fail";
  if (username === ACCOUNT && password.length > 0 && password.length <= MAX_PASSWORD_LEN) {
    outcome = await pamAuthenticate(password);
  }
  password = "";

  if (outcome === "busy") {
    return json(request, { error: "Authentication busy" }, 429, { "retry-after": "5" });
  }

  if (outcome !== "ok") {
    recordFailure(ip, Date.now());
    log("login rejected", ip);
    await equalizeLoginTiming(started);
    // Deliberately identical for unknown username and wrong password.
    return json(request, { error: "Wrong password" }, 401);
  }

  // Rotate on login: a pre-existing session cookie is discarded (fixation),
  // and a good password clears this client's failure history.
  failures.delete(ip);
  const previous = cookie(request);
  if (previous) sessions.delete(previous);
  const key = issueSession();
  log("login accepted", ip);
  return json(request, { ok: true }, 200, {
    "set-cookie": sessionCookie(request, key, SESSION_TTL_MS / 1000),
  });
};

const handleLogout = (request: Request): Response => {
  if (!sameOriginOk(request)) return json(request, { error: "Cross-site request rejected" }, 403);
  const key = cookie(request);
  if (key) {
    sessions.delete(key);
    if (viewer?.data.session === key) viewer.close(4002, "Session logged out");
  }
  return json(request, { ok: true }, 200, {
    "set-cookie": sessionCookie(request, "", 0),
  });
};

const tokenMatches = (provided: string, token: string): boolean => {
  const expected = Buffer.from("Bearer " + token);
  const actual = Buffer.from(provided);
  return actual.length === expected.length && timingSafeEqual(actual, expected);
};

const PUBLIC_FILES = new Set(["login.html", "login.css", "login.js"]);
/** Allowlist: a flat file name with a known extension; no slashes or dot-dot. */
const WEB_FILE = /^[a-z0-9][a-z0-9_-]*\.(?:html|css|js)$/i;

const serveStatic = async (request: Request, pathname: string, authenticated: boolean): Promise<Response> => {
  let name: string;
  if (pathname === "/") name = authenticated ? "index.html" : "login.html";
  else if (pathname.startsWith("/web/")) name = pathname.slice("/web/".length);
  else return text(request, "Not found", 404);
  if (!WEB_FILE.test(name)) return text(request, "Not found", 404);
  if (!authenticated && !PUBLIC_FILES.has(name)) return text(request, "Unauthorized", 401);
  const file = Bun.file(join(WEB_ROOT, name));
  if (!(await file.exists())) return text(request, "Not found", 404);
  return new Response(file, { headers: { ...securityHeaders(request), "content-type": mimeFor(name) } });
};

/* ------------------------------------------------------------------ */
/* periodic maintenance                                               */
/* ------------------------------------------------------------------ */

const sweeper = setInterval(() => {
  const now = Date.now();
  pruneSessions(now);
  pruneFailures(now);
  const watch = viewer?.data.session;
  if (watch) {
    const expires = sessions.get(watch);
    if (expires === undefined || expires <= now) viewer?.close(4002, "Session expired");
  }
}, SWEEP_INTERVAL_MS);
// Do not keep the process alive just for the janitor.
(sweeper as unknown as { unref?: () => void }).unref?.();

/* ------------------------------------------------------------------ */
/* server                                                             */
/* ------------------------------------------------------------------ */


/* ------------------------------------------------------------------ */
/* ICE server discovery (STUN always; TURN credentials when configured)*/
/* ------------------------------------------------------------------ */

/*
 * Media stays peer to peer whenever the networks allow it. When both ends sit
 * behind carrier grade NAT a direct path is impossible, so the browser and the
 * host additionally receive short lived TURN credentials for Cloudflare's
 * relay. DTLS-SRTP is end to end, so the relay only forwards encrypted packets.
 * The TURN key secret lives in .secrets/turn-key and never leaves this process.
 */
const STUN_URLS = ["stun:stun.cloudflare.com:3478", "stun:stun.l.google.com:19302"];
const TURN_TTL_SECONDS = 6 * 60 * 60;

type IceServerConfig = { urls: string | string[]; username?: string; credential?: string };

const turnKey: { id: string; secret: string } | undefined = (() => {
  try {
    const lines = readFileSync(join(ROOT, ".secrets/turn-key"), "utf8").trim().split("\n");
    if (lines.length >= 2 && lines[0].trim() && lines[1].trim()) {
      return { id: lines[0].trim(), secret: lines[1].trim() };
    }
  } catch { /* TURN is optional */ }
  return undefined;
})();

let turnCached: IceServerConfig | null = null;
let turnFetchedAt = 0;
let turnLastAttempt = 0;
let turnInflight: Promise<IceServerConfig | null> | undefined;

const fetchTurnCredentials = async (): Promise<IceServerConfig | null> => {
  if (!turnKey) return null;
  try {
    const response = await fetch(
      `https://rtc.live.cloudflare.com/v1/turn/keys/${turnKey.id}/credentials/generate`,
      {
        method: "POST",
        headers: { authorization: "Bearer " + turnKey.secret, "content-type": "application/json" },
        body: JSON.stringify({ ttl: TURN_TTL_SECONDS }),
        signal: AbortSignal.timeout(6000),
      },
    );
    if (!response.ok) {
      console.error("[ice] TURN credential request failed:", response.status);
      return null;
    }
    const body = (await response.json()) as { iceServers?: IceServerConfig };
    const servers = body?.iceServers;
    if (!servers || !servers.urls || !servers.username || !servers.credential) return null;
    return { urls: servers.urls, username: servers.username, credential: servers.credential };
  } catch (error) {
    console.error("[ice] TURN credential error:", String(error).slice(0, 120));
    return null;
  }
};

const iceServersPayload = async (): Promise<{ iceServers: IceServerConfig[]; turn: boolean; ttl: number }> => {
  const now = Date.now();
  const half = (TURN_TTL_SECONDS * 1000) / 2;
  if (turnKey && now - turnFetchedAt > half && now - turnLastAttempt > 30_000) {
    turnLastAttempt = now;
    turnInflight ??= fetchTurnCredentials().finally(() => { turnInflight = undefined; });
    const fresh = await turnInflight;
    if (fresh) { turnCached = fresh; turnFetchedAt = now; }
  }
  const iceServers: IceServerConfig[] = [{ urls: STUN_URLS }];
  if (turnCached) iceServers.push(turnCached);
  return { iceServers, turn: !!turnCached, ttl: TURN_TTL_SECONDS };
};

const send = (ws: ServerWebSocket<Peer> | undefined, packet: unknown) => {
  if (ws && ws.readyState === 1) ws.send(JSON.stringify(packet));
};

const state = () => ({ type: "status", host: !!host, viewer: !!viewer });

Bun.serve<Peer>({
  hostname: BIND,
  port: PORT,
  maxRequestBodySize: 64 * 1024,
  async fetch(request, server) {
    const url = new URL(request.url);
    const pathname = url.pathname;
    const method = request.method.toUpperCase();

    if (pathname === "/health") return json(request, { ok: true });

    if (pathname === "/api/login") {
      if (method !== "POST") return json(request, { error: "Method not allowed" }, 405, { allow: "POST" });
      return handleLogin(request, server);
    }
    if (pathname === "/api/logout") {
      if (method !== "POST") return json(request, { error: "Method not allowed" }, 405, { allow: "POST" });
      return handleLogout(request);
    }
    if (pathname === "/api/status") {
      if (method !== "GET") return json(request, { error: "Method not allowed" }, 405, { allow: "GET" });
      return authorized(request) ? json(request, { host: !!host, viewer: !!viewer }) : json(request, { error: "Unauthorized" }, 401);
    }
    if (pathname === "/api/ice") {
      if (method !== "GET") return json(request, { error: "Method not allowed" }, 405, { allow: "GET" });
      const isHost = tokenMatches(request.headers.get("authorization") ?? "", hostToken);
      if (!isHost && !authorized(request)) return json(request, { error: "Unauthorized" }, 401);
      return json(request, await iceServersPayload());
    }
    if (pathname === "/ws/host") {
      if (method !== "GET") return text(request, "Method not allowed", 405);
      if (!tokenMatches(request.headers.get("authorization") ?? "", hostToken)) return text(request, "Unauthorized", 401);
      if (server.upgrade(request, { data: { role: "host" } })) return;
      return text(request, "Upgrade required", 426);
    }
    if (pathname === "/ws/viewer") {
      if (method !== "GET") return text(request, "Method not allowed", 405);
      const session = authorized(request);
      if (!session) return text(request, "Unauthorized", 401);
      if (!originMatches(request)) return text(request, "Forbidden origin", 403);
      if (server.upgrade(request, { data: { role: "viewer", session: session.key } })) return;
      return text(request, "Upgrade required", 426);
    }
    if (pathname.startsWith("/api/")) return json(request, { error: "Not found" }, 404);

    return serveStatic(request, pathname, !!authorized(request));
  },
  websocket: {
    maxPayloadLength: 256 * 1024,
    idleTimeout: 120,
    open(ws) {
      if (ws.data.role === "host") {
        host?.close(4001, "New host connected");
        host = ws;
      } else {
        // Defense in depth: the session may have expired between upgrade and open.
        const session = ws.data.session;
        if (!session || !sessions.has(session)) {
          ws.close(4002, "Unauthorized");
          return;
        }
        viewer?.close(4001, "New viewer connected");
        viewer = ws;
      }
      send(host, state());
      send(viewer, state());
    },
    message(ws, message) {
      if (typeof message !== "string" || message.length > 256 * 1024) return;
      if (ws.data.role === "viewer" && ws.data.session) {
        const expires = sessions.get(ws.data.session);
        if (expires === undefined || expires <= Date.now()) {
          sessions.delete(ws.data.session);
          ws.close(4002, "Session expired");
          return;
        }
      }
      let data: any;
      try { data = JSON.parse(message); } catch { return; }
      if (!data || typeof data !== "object") return;
      if (!["offer", "answer", "candidate", "ready", "error", "ping"].includes(data.type)) return;
      if (data.type === "ping") { send(ws, { type: "pong", at: Date.now() }); return; }
      if (DEBUG) {
        const detail = data.type === "candidate"
          ? (data.candidate?.candidate || "").slice(0, 120)
          : data.type === "offer" || data.type === "answer" ? "sdp" : "";
        log(ws.data.role, "->", data.type, detail, "host:", !!host, "viewer:", !!viewer);
      }
      if (ws.data.role === "viewer" && (data.type === "offer" || data.type === "candidate")) send(host, data);
      if (ws.data.role === "host" && ["answer", "candidate", "ready", "error"].includes(data.type)) send(viewer, data);
    },
    close(ws) {
      if (ws.data.role === "host" && host === ws) { host = undefined; send(viewer, { type: "reset" }); }
      if (ws.data.role === "viewer" && viewer === ws) { viewer = undefined; send(host, { type: "reset" }); }
      send(host, state());
      send(viewer, state());
    },
  },
});

console.log("SwayRTC signaling listening on http://" + BIND + ":" + PORT + " (loopback-only by default)");
console.log("Media never passes through this Bun server.");
