/* SwayRTC sign-in: PAM-backed login form.
 * - exactly one in-flight request (no double submit)
 * - friendly errors, never reflects raw server text
 * - password is never stored anywhere except the input element */
(() => {
  "use strict";

  const form = document.getElementById("login");
  const username = document.getElementById("username");
  const password = document.getElementById("pass");
  const submit = document.getElementById("submit");
  const label = submit && submit.querySelector(".label");
  const output = document.getElementById("error");

  if (!form || !username || !password || !submit || !label || !output) return;

  const IDLE_LABEL = "Connect to desktop";
  const BUSY_LABEL = "Connecting…";
  const MESSAGES = {
    400: "The sign-in request was malformed.",
    401: "Incorrect username or password.",
    403: "Request blocked: it did not come from this site.",
    413: "That password is too long.",
    429: "Too many attempts. Wait a few minutes, then try again.",
  };

  let busy = false;

  const showError = (message) => { output.textContent = message || ""; };

  const setBusy = (value) => {
    busy = value;
    form.setAttribute("aria-busy", String(value));
    submit.disabled = value;
    label.textContent = value ? BUSY_LABEL : IDLE_LABEL;
  };

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    if (busy) return;

    const user = username.value.trim();
    const secret = password.value;
    if (!user) { showError("Enter your PC username."); username.focus(); return; }
    if (!secret) { showError("Enter your PC login password."); password.focus(); return; }

    showError("");
    setBusy(true);
    try {
      const response = await fetch("/api/login", {
        method: "POST",
        headers: { "content-type": "application/json" },
        credentials: "same-origin",
        cache: "no-store",
        body: JSON.stringify({ username: user, password: secret }),
      });
      if (response.ok) {
        location.replace("/");
        return;
      }
      const message = MESSAGES[response.status] || "Sign-in failed (HTTP " + response.status + ").";
      showError(message);
      if (response.status === 401 || response.status === 400) {
        password.focus();
        password.select();
      }
    } catch {
      showError("Cannot reach the SwayRTC server. Check your connection.");
    } finally {
      setBusy(false);
    }
  });

  for (const input of [username, password]) {
    input.addEventListener("input", () => { if (!busy) showError(""); });
  }
})();
