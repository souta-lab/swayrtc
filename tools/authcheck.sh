#!/usr/bin/env bash
# SwayRTC auth hardening checks against the isolated test env (port 8784).
# Never touches production 8777.
set -uo pipefail

BASE=http://127.0.0.1:8784
USER_NAME=swayrtctest
# Test account password: from the environment or .secrets/test-password (gitignored).
USER_PASS="${USER_PASS:-$(cat "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/.secrets/test-password" 2>/dev/null || true)}"
: "${USER_PASS:?USER_PASS is not set and .secrets/test-password was not found}"
ORIGIN=$BASE
JAR=/tmp/authjar
BODY=/tmp/authbody
PASS=0
FAIL=0

rm -f "$JAR"

req() { # req <method> <path> [curl args...] -> echoes status code, body in $BODY
  local method=$1 path=$2; shift 2
  curl -sS -m 8 -o "$BODY" -w '%{http_code}' -X "$method" "$BASE$path" "$@"
}

check() { # check <label> <expected> <actual>
  if [ "$2" = "$3" ]; then PASS=$((PASS+1)); printf 'ok   %-58s %s\n' "$1" "$3";
  else FAIL=$((FAIL+1)); printf 'FAIL %-58s expected=%s actual=%s\n' "$1" "$2" "$3"; fi
}

hdr() { # hdr <path> <header> [curl args]
  local path=$1 header=$2; shift 2
  curl -sS -m 8 -D - -o /dev/null "$BASE$path" "$@" | tr -d '\r' | awk -v h="$header" 'BEGIN{IGNORECASE=1} tolower($0) ~ ("^" tolower(h) ":")' | head -3
}

# ------------------------------------------------------------------ 1. static
code=$(req GET /)
check "GET / anonymous serves login page" 200 "$code"
grep -q 'Sign in' "$BODY" && check "login page title present" yes yes || check "login page title present" yes no

code=$(req GET /web/login.css)
check "GET /web/login.css anonymous" 200 "$code"
code=$(req GET /web/login.js)
check "GET /web/login.js anonymous" 200 "$code"
code=$(req GET /web/index.html)
check "GET /web/index.html anonymous blocked" 401 "$code"
code=$(req GET /web/app.js)
check "GET /web/app.js anonymous blocked" 401 "$code"
code=$(req GET /web/style.css)
check "GET /web/style.css anonymous blocked" 401 "$code"

# path traversal / unknown files
code=$(curl -sS -m 8 --path-as-is -o "$BODY" -w '%{http_code}' "$BASE/../server.ts")
check "path traversal /../server.ts" 404 "$code"
code=$(curl -sS -m 8 --path-as-is -o "$BODY" -w '%{http_code}' "$BASE/web/../../server.ts")
check "path traversal /web/../../server.ts" 404 "$code"
code=$(req GET '/web/%2e%2e%2fserver.ts')
check "encoded traversal /web/%2e%2e%2fserver.ts" 404 "$code"
code=$(req GET /web/..%2F..%2F.secrets/host-token)
check "encoded traversal to .secrets" 404 "$code"
code=$(req GET /web/nope.js)
check "unknown /web file anonymous (no existence leak)" 401 "$code"

# health information minimisation
code=$(req GET /health)
check "GET /health" 200 "$code"
check "health body minimal" '{"ok":true}' "$(cat "$BODY")"

# ------------------------------------------------------------------ 2. CSRF
code=$(req POST /api/login -H 'Origin: https://evil.example' -H 'content-type: application/json' -d "{\"username\":\"$USER_NAME\",\"password\":\"$USER_PASS\"}")
check "login cross-origin Origin rejected" 403 "$code"
code=$(req POST /api/login -H 'Referer: https://evil.example/x' -H 'content-type: application/json' -d "{\"username\":\"$USER_NAME\",\"password\":\"$USER_PASS\"}")
check "login cross-origin Referer rejected" 403 "$code"
code=$(req POST /api/logout -H 'Origin: https://evil.example')
check "logout cross-origin rejected" 403 "$code"
code=$(req POST /api/login -H 'Origin: null' -H 'content-type: application/json' -d '{}')
check "login Origin: null rejected" 403 "$code"

# ------------------------------------------------------------------ 3. login success
code=$(req POST /api/login -H "Origin: $ORIGIN" -H 'content-type: application/json' -c "$JAR" -d "{\"username\":\"$USER_NAME\",\"password\":\"$USER_PASS\"}")
check "login correct password (same-origin)" 200 "$code"
set_cookie=$(curl -sS -m 8 -D - -o /dev/null -X POST "$BASE/api/login" -H "Origin: $ORIGIN" -H 'content-type: application/json' -d "{\"username\":\"$USER_NAME\",\"password\":\"$USER_PASS\"}" -c "$JAR" | tr -d '\r' | grep -i '^set-cookie:' | head -1)
echo "     set-cookie: $set_cookie"
case "$set_cookie" in *HttpOnly*) check "cookie HttpOnly" yes yes;; *) check "cookie HttpOnly" yes no;; esac
case "$set_cookie" in *SameSite=Strict*) check "cookie SameSite=Strict" yes yes;; *) check "cookie SameSite=Strict" yes no;; esac
case "$set_cookie" in *Max-Age=43200*) check "cookie Max-Age=12h" yes yes;; *) check "cookie Max-Age=12h" yes no;; esac
case "$set_cookie" in *Secure*) check "no Secure on plain http" no yes;; *) check "no Secure on plain http" no no;; esac

# no-Origin curl (non-browser) must also work
JAR2=/tmp/authjar2; rm -f "$JAR2"
code=$(req POST /api/login -H 'content-type: application/json' -c "$JAR2" -d "{\"username\":\"$USER_NAME\",\"password\":\"$USER_PASS\"}")
check "login correct password (no Origin header)" 200 "$code"

# secure cookie behind TLS terminator
sc=$(curl -sS -m 8 -D - -o /dev/null -X POST "$BASE/api/login" -H 'x-forwarded-proto: https' -H "Origin: $ORIGIN" -H 'content-type: application/json' -d "{\"username\":\"$USER_NAME\",\"password\":\"$USER_PASS\"}" | tr -d '\r' | grep -i '^set-cookie:' | head -1)
case "$sc" in *Secure*) check "Secure cookie when x-forwarded-proto=https" yes yes;; *) check "Secure cookie when x-forwarded-proto=https" yes no;; esac
hsts=$(hdr /health strict-transport-security -H 'x-forwarded-proto: https')
[ -n "$hsts" ] && check "HSTS over https" yes yes || check "HSTS over https" yes no
hsts_plain=$(hdr /health strict-transport-security)
[ -z "$hsts_plain" ] && check "no HSTS over plain http" yes yes || check "no HSTS over plain http" yes no

# ------------------------------------------------------------------ 4. login failures
# Each failing attempt uses its own client bucket (cf-connecting-ip is trusted
# from loopback, like the tunnel) so the shared loopback bucket stays clean.
BUCKET=198.51.100.1
fail_login() { # fail_login <bucket> <json>
  curl -sS -m 8 -o "$BODY" -w '%{http_code}' -X POST "$BASE/api/login" -H "Origin: $ORIGIN" \
    -H "cf-connecting-ip: $1" -H 'content-type: application/json' -d "$2"
}
code=$(fail_login "$BUCKET" "{\"username\":\"$USER_NAME\",\"password\":\"definitely-wrong\"}")
check "login wrong password" 401 "$code"
check "wrong password body" '{"error":"Wrong password"}' "$(cat "$BODY")"
code=$(fail_login 198.51.100.2 "{\"username\":\"$USER_NAME\",\"password\":\"\"}")
check "login empty password" 401 "$code"
code=$(fail_login 198.51.100.3 "{\"username\":\"root\",\"password\":\"$USER_PASS\"}")
check "login username mismatch" 401 "$code"
check "username mismatch same error" '{"error":"Wrong password"}' "$(cat "$BODY")"
code=$(fail_login 198.51.100.4 '{"username":"","password":""}')
check "login empty body fields" 401 "$code"
code=$(fail_login 198.51.100.5 'not-json')
check "login malformed JSON" 401 "$code"
code=$(fail_login 198.51.100.6 '')
check "login POST without body" 401 "$code"

# timing: unknown username vs wrong password (both >= 300ms floor)
t_wrong=$(curl -sS -m 8 -o /dev/null -w '%{time_total}' -X POST "$BASE/api/login" -H "Origin: $ORIGIN" -H 'cf-connecting-ip: 198.51.100.20' -H 'content-type: application/json' -d "{\"username\":\"$USER_NAME\",\"password\":\"nope-nope\"}")
t_user=$(curl -sS -m 8 -o /dev/null -w '%{time_total}' -X POST "$BASE/api/login" -H "Origin: $ORIGIN" -H 'cf-connecting-ip: 198.51.100.21' -H 'content-type: application/json' -d "{\"username\":\"nosuchuser\",\"password\":\"nope-nope\"}")
echo "     timing wrong-password=${t_wrong}s unknown-user=${t_user}s (pam_faildelay is 3s in this env)"
awk -v a="$t_wrong" -v b="$t_user" 'BEGIN{ exit !(a>=1.0 && b>=0.28) }' \
  && check "wrong password goes through PAM (faildelay), floor on unknown user" yes yes \
  || check "wrong password goes through PAM (faildelay), floor on unknown user" yes no
awk -v b="$t_user" 'BEGIN{ exit !(b < 1.5) }' \
  && check "foreign username never invokes PAM" yes yes || check "foreign username never invokes PAM" yes no

# oversized body
code=$(head -c 6000 /dev/zero | tr '\0' 'a' | curl -sS -m 8 -o /dev/null -w '%{http_code}' -X POST "$BASE/api/login" -H "Origin: $ORIGIN" -H 'content-type: application/json' --data-binary @-)
check "oversized login payload" 413 "$code"

# ------------------------------------------------------------------ 5. method handling
code=$(req GET /api/login)
check "GET /api/login -> 405" 405 "$code"
code=$(req GET /api/logout)
check "GET /api/logout -> 405" 405 "$code"

# ------------------------------------------------------------------ 6. authorization
code=$(curl -sS -m 8 -o "$BODY" -w '%{http_code}' -b "$JAR" "$BASE/")
check "GET / with session -> app shell" 200 "$code"
code=$(curl -sS -m 8 -o "$BODY" -w '%{http_code}' -b "$JAR" "$BASE/api/status")
check "GET /api/status with session" 200 "$code"
case "$(cat "$BODY")" in
  '{"host":true,"viewer":true}'|'{"host":true,"viewer":false}') check "status body shape" yes yes;;
  *) check "status body shape" '{"host":boolean,"viewer":boolean}' "$(cat "$BODY")";;
esac
code=$(req GET /api/status)
check "GET /api/status without session" 401 "$code"
code=$(curl -sS -m 8 -o /dev/null -w '%{http_code}' -b "$JAR" "$BASE/web/app.js")
check "GET /web/app.js with session" 200 "$code"
code=$(curl -sS -m 8 -o /dev/null -w '%{http_code}' -b "$JAR" "$BASE/web/style.css")
check "GET /web/style.css with session" 200 "$code"

# ------------------------------------------------------------------ 7. websockets
ws_hdr=(-H 'Connection: Upgrade' -H 'Upgrade: websocket' -H 'Sec-WebSocket-Version: 13' -H 'Sec-WebSocket-Key: x3JJHMbDL1EzLkh9GBhXDw==')
code=$(curl -sS -m 8 -o /dev/null -w '%{http_code}' "${ws_hdr[@]}" "$BASE/ws/viewer")
check "/ws/viewer without cookie" 401 "$code"
code=$(curl -sS -m 8 -o /dev/null -w '%{http_code}' "${ws_hdr[@]}" -b "$JAR" -H 'Origin: https://evil.example' "$BASE/ws/viewer")
check "/ws/viewer origin mismatch" 403 "$code"
code=$(curl -sS -m 8 -o /dev/null -w '%{http_code}' "${ws_hdr[@]}" -b "$JAR" "$BASE/ws/viewer")
check "/ws/viewer without Origin" 403 "$code"
first=$(curl -sS -m 2 -i "${ws_hdr[@]}" -b "$JAR" -H "Origin: $ORIGIN" "$BASE/ws/viewer" 2>/dev/null | head -1 | tr -d '\r')
check "/ws/viewer authorized upgrade" "HTTP/1.1 101 Switching Protocols" "$first"

code=$(curl -sS -m 8 -o /dev/null -w '%{http_code}' "${ws_hdr[@]}" "$BASE/ws/host")
check "/ws/host without bearer" 401 "$code"
code=$(curl -sS -m 8 -o /dev/null -w '%{http_code}' "${ws_hdr[@]}" -H 'Authorization: Bearer wrong-token' "$BASE/ws/host")
check "/ws/host wrong bearer" 401 "$code"

# ------------------------------------------------------------------ 8. logout
code=$(curl -sS -m 8 -o "$BODY" -w '%{http_code}' -b "$JAR" -c "$JAR" -X POST -H "Origin: $ORIGIN" "$BASE/api/logout")
check "logout with session" 200 "$code"
sc=$(curl -sS -m 8 -D - -o /dev/null -b "$JAR" -X POST -H "Origin: $ORIGIN" "$BASE/api/logout" | tr -d '\r' | grep -i '^set-cookie:' | head -1)
case "$sc" in *Max-Age=0*) check "logout clears cookie" yes yes;; *) check "logout clears cookie" yes no;; esac
code=$(curl -sS -m 8 -o /dev/null -w '%{http_code}' -b "$JAR" "$BASE/api/status")
check "status after logout" 401 "$code"
code=$(curl -sS -m 8 -o /dev/null -w '%{http_code}' "${ws_hdr[@]}" -b "$JAR" -H "Origin: $ORIGIN" "$BASE/ws/viewer")
check "viewer ws after logout" 401 "$code"

# ------------------------------------------------------------------ 9. session rotation
JAR3=/tmp/authjar3; rm -f "$JAR3"
code=$(req POST /api/login -H "Origin: $ORIGIN" -H 'content-type: application/json' -c "$JAR3" -d "{\"username\":\"$USER_NAME\",\"password\":\"$USER_PASS\"}")
check "re-login with fresh jar" 200 "$code"
old=$(grep sr_session "$JAR3" | awk '{print $7}')
code=$(curl -sS -m 8 -o /dev/null -w '%{http_code}' -b "$JAR3" -c "$JAR3" -X POST -H "Origin: $ORIGIN" -H 'content-type: application/json' -d "{\"username\":\"$USER_NAME\",\"password\":\"$USER_PASS\"}" "$BASE/api/login")
check "second login rotates session" 200 "$code"
new=$(grep sr_session "$JAR3" | awk '{print $7}')
[ "$old" != "$new" ] && check "session key changed" yes yes || check "session key changed" yes no
code=$(curl -sS -m 8 -o /dev/null -w '%{http_code}' -H "Cookie: sr_session=$old" "$BASE/api/status")
check "old session invalid after rotation" 401 "$code"

# ------------------------------------------------------------------ 10. rate limiting
IPV4=203.0.113.9
codes=""
for i in 1 2 3 4 5; do
  c=$(curl -sS -m 8 -o /dev/null -w '%{http_code}' -X POST "$BASE/api/login" -H "Origin: $ORIGIN" -H "cf-connecting-ip: $IPV4" -H 'content-type: application/json' -d "{\"username\":\"$USER_NAME\",\"password\":\"bad-$i\"}")
  codes="$codes $c"
done
echo "     first five failure codes:$codes"
[ "$codes" = " 401 401 401 401 401" ] && check "5 failures are 401" yes yes || check "5 failures are 401" yes no
resp=$(curl -sS -m 8 -D /tmp/rl_hdr -o /dev/null -w '%{http_code}' -X POST "$BASE/api/login" -H "Origin: $ORIGIN" -H "cf-connecting-ip: $IPV4" -H 'content-type: application/json' -d "{\"username\":\"$USER_NAME\",\"password\":\"$USER_PASS\"}")
check "6th attempt locked (even correct password)" 429 "$resp"
grep -qi '^retry-after: [0-9]' /tmp/rl_hdr && check "429 has Retry-After" yes yes || check "429 has Retry-After" yes no
code=$(curl -sS -m 8 -o /dev/null -w '%{http_code}' -X POST "$BASE/api/login" -H "Origin: $ORIGIN" -H "cf-connecting-ip: 203.0.113.10" -H 'content-type: application/json' -d "{\"username\":\"$USER_NAME\",\"password\":\"$USER_PASS\"}")
check "different IPv4 unaffected by other lock" 200 "$code"

# IPv6 /64 grouping: rotate inside the same /64 and hit the shared bucket
codes6=""
for a in 2001:db8:aa:bb::1 2001:db8:aa:bb::2 2001:db8:aa:bb::3 2001:db8:aa:bb::4 2001:db8:aa:bb::5; do
  c=$(curl -sS -m 8 -o /dev/null -w '%{http_code}' -X POST "$BASE/api/login" -H "Origin: $ORIGIN" -H "cf-connecting-ip: $a" -H 'content-type: application/json' -d '{"username":"nosuch","password":"bad"}')
  codes6="$codes6 $c"
done
c=$(curl -sS -m 8 -o /dev/null -w '%{http_code}' -X POST "$BASE/api/login" -H "Origin: $ORIGIN" -H "cf-connecting-ip: 2001:db8:aa:bb::6" -H 'content-type: application/json' -d '{"username":"nosuch","password":"bad"}')
echo "     ipv6 /64 attempt codes:$codes6 -> $c"
[ "$c" = "429" ] && check "IPv6 failures grouped by /64" yes yes || check "IPv6 failures grouped by /64" yes no

# ------------------------------------------------------------------ 10b. production-equivalence headers
prod=(-H 'Origin: https://swayrtc.example.com' -H 'x-forwarded-proto: https')
code=$(curl -sS -m 8 -D /tmp/prod_hdr -o /dev/null -w '%{http_code}' -X POST "$BASE/api/login" \
  -H 'Host: swayrtc.example.com' -H 'x-forwarded-proto: https' -H 'Origin: https://swayrtc.example.com' \
  -H 'content-type: application/json' -c /tmp/authjar_prod \
  -d "{\"username\":\"$USER_NAME\",\"password\":\"$USER_PASS\"}")
check "login through tunnel-like headers (public Host)" 200 "$code"
sc=$(grep -i '^set-cookie:' /tmp/prod_hdr | tr -d '\r' | head -1)
case "$sc" in *Secure*) check "tunnel login sets Secure cookie" yes yes;; *) check "tunnel login sets Secure cookie" yes no;; esac
sess=$(grep -io 'sr_session=[0-9a-f]\{64\}' /tmp/prod_hdr | head -1 | tr -d '\r')
first=$(curl -sS -m 2 -i "${ws_hdr[@]}" -H "Cookie: $sess" \
  -H 'Origin: https://swayrtc.example.com' -H 'X-Forwarded-Host: swayrtc.example.com' \
  "$BASE/ws/viewer" 2>/dev/null | head -1 | tr -d '\r')
check "/ws/viewer through tunnel-like headers" "HTTP/1.1 101 Switching Protocols" "$first"

# authenticated access to a missing file is a plain 404
code=$(curl -sS -m 8 -o /dev/null -w '%{http_code}' -H "Cookie: $sess" -H 'X-Forwarded-Host: swayrtc.example.com' "$BASE/web/nope.js")
check "authenticated unknown /web file -> 404" 404 "$code"
code=$(curl -sS -m 8 -o "$BODY" -w '%{http_code}' -H "Cookie: $sess" "$BASE/web/index.html")
check "authenticated index.html" 200 "$code"

# success clears the failure counter for that client
for i in 1 2 3 4; do
  curl -sS -m 8 -o /dev/null -X POST "$BASE/api/login" -H "Origin: $ORIGIN" -H 'cf-connecting-ip: 198.51.100.30' \
    -H 'content-type: application/json' -d "{\"username\":\"$USER_NAME\",\"password\":\"bad-$i\"}"
done
c=$(curl -sS -m 8 -o /dev/null -w '%{http_code}' -X POST "$BASE/api/login" -H "Origin: $ORIGIN" -H 'cf-connecting-ip: 198.51.100.30' -H 'content-type: application/json' -d "{\"username\":\"$USER_NAME\",\"password\":\"$USER_PASS\"}")
check "successful login after 4 failures" 200 "$c"
c1=$(curl -sS -m 8 -o /dev/null -w '%{http_code}' -X POST "$BASE/api/login" -H "Origin: $ORIGIN" -H 'cf-connecting-ip: 198.51.100.30' -H 'content-type: application/json' -d '{"username":"x","password":"bad"}')
c2=$(curl -sS -m 8 -o /dev/null -w '%{http_code}' -X POST "$BASE/api/login" -H "Origin: $ORIGIN" -H 'cf-connecting-ip: 198.51.100.30' -H 'content-type: application/json' -d '{"username":"x","password":"bad"}')
check "success reset the failure counter" "401 401" "$c1 $c2"

# ------------------------------------------------------------------ 11. hygiene
if grep -qa '$USER_PASS\|definitely-wrong\|bad-3' /tmp/swayrtc-test-8784/bun.log; then
  check "password never appears in server log" yes no
else
  check "password never appears in server log" yes yes
fi
if pgrep -u swayrtctest -x pamtester >/dev/null 2>&1; then
  check "no leftover pamtester processes" yes no
else
  check "no leftover pamtester processes" yes yes
fi

echo
echo "PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ]
