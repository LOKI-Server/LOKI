// Setup, sign-in, registration, password reset, and the Windows startup review.
import { $, api, esc, modal, store } from "../lib/core.js";
import { button, check, field, heading } from "../lib/ui.js";

const futhark = "ᚠ ᚢ ᚦ ᚨ ᚱ ᚲ ᚷ ᚹ ᚺ ᚾ ᛁ ᛃ ᛇ ᛈ ᛉ ᛊ ᛏ ᛒ ᛖ ᛗ ᛚ ᛜ ᛞ ᛟ";
// The twenty-four runes of the Elder Futhark, circling the Algiz rune of protection.
const sigil = `<div class="sigil" aria-hidden="true"><svg class="rune-ring" viewBox="0 0 200 200"><defs><path id="ring-path" d="M100 100m-73 0a73 73 0 1 1 146 0a73 73 0 1 1-146 0"/></defs><circle cx="100" cy="100" r="94"/><circle cx="100" cy="100" r="62"/><text><textPath href="#ring-path" textLength="455">${futhark}</textPath></text></svg><span class="rune">ᛉ</span></div>`;

export function setupLogin(message) {
  try { sessionStorage.setItem("loki-setup-notice", message); } catch {}
  location.assign("/login");
}

export function confirmStartupReplacement(details) {
  return new Promise(resolve => {
    const dialog = $("#dialog");
    let settled = false;
    const dismissed = () => { if (!settled) { settled = true; resolve(null); } };
    modal("Replace the old startup entry?", `<p>Windows is set to start another copy of LOKI when you sign in.</p><dl class="paths"><dt>Currently starts</dt><dd class="mono">${esc(details.existing_executable)}</dd><dt>Start this copy instead</dt><dd class="mono">${esc(details.current_executable)}</dd><dt>Data folder used by this copy</dt><dd class="mono">${esc(details.data_directory)}</dd></dl><p class="help">Replacing the startup entry changes which executable runs at sign-in. Your data stays in the folder shown above. The old entry is replaced so both copies do not start.</p><div class="actions"><button type="button" id="startup-replace">Replace startup entry</button><button type="button" class="subtle" id="startup-cancel">Cancel</button></div>`);
    dialog.addEventListener("close", dismissed, { once: true });
    const finish = value => {
      if (settled) return;
      settled = true;
      dialog.removeEventListener("close", dismissed);
      dialog.close();
      resolve(value);
    };
    $("#startup-replace").addEventListener("click", () => finish(details.confirmation), { once: true });
    $("#startup-cancel").addEventListener("click", () => finish(null), { once: true });
  });
}

export async function submitStartupAware(send) {
  let confirmation;
  for (let attempt = 0; attempt < 3; attempt++) {
    const result = await send(confirmation);
    if (result.requires_confirmation !== "startup") return result;
    confirmation = await confirmStartupReplacement(result.startup);
    if (!confirmation) return null;
  }
  throw new Error("The startup entry kept changing. Try saving again.");
}

export function startupControl(settings) {
  const startup = store.state?.startup, other = startup?.needs_confirmation;
  return check("startup", "Start LOKI when I sign in to this computer", settings.startup && !other) + (other ? `<p class="help">Windows currently starts another LOKI copy: <span class="mono">${esc(startup.registered_executable)}</span>. Enable startup and save to review replacing it with this copy.</p>` : startup?.error ? `<p class="help">${esc(startup.error)}</p>` : "");
}

export async function submitSetup(data) {
  let response;
  try {
    response = await submitStartupAware(confirmation => api("/api/setup", { ...data, startup_confirmation: confirmation }));
    if (!response) return;
  } catch (error) {
    try {
      store.session = await api("/api/session");
      if (!store.session.setup) { setupLogin("LOKI is already set up. Sign in with the owner username and password you chose."); return; }
    } catch {}
    throw error;
  }
  const message = response.warning || "Your LOKI owner account is ready. Sign in with the username and password you chose.";
  if (response.restarting) {
    $("#main").innerHTML = heading("Your universe is ready.", "LOKI is restarting in the selected data directory. The sign-in page will open shortly.");
    setTimeout(() => setupLogin(message), 3000);
  } else setupLogin(message);
}

const titles = { setup: "The story starts here.", login: "Welcome to LOKI.", register: "Find your place.", reset: "A fresh beginning." };
const submits = { setup: "Create your universe", login: "Sign in", register: "Request account", reset: "Set password" };

/** The app a sign-in continues to, when LOKI's sign-in page was opened for one. */
function signingInTo() {
  const query = new URLSearchParams(location.search);
  return query.get("next")?.startsWith("/oidc/authorize?") ? query.get("app") || "" : "";
}

/** Where an app's "Also sign out" leads: signing out here ends its apps' sessions too. */
export function signOutPage() {
  const { session } = store, server = esc(session.name || "LOKI");
  if (!session.user) return `<section class="card auth-card">${sigil}<div class="eyebrow">${server}</div><h1>You’re signed out.</h1><p>You’re signed out of ${server} and its apps on this device.</p><div class="actions"><a class="button full" href="/login">Sign in</a></div></section>`;
  return `<section class="card auth-card">${sigil}<div class="eyebrow">${server}</div><h1>Sign out of ${server}?</h1><p>This also signs you out of the apps you opened from this device.</p><div class="actions">${button("Sign out", "logout", "", "", "full", "logout")}</div><hr class="divider"><div class="auth-footer"><a href="/">Stay signed in</a></div></section>`;
}

export function authPage(kind) {
  const session = store.session;
  const app = kind === "login" ? signingInTo() : "";
  let content = "";
  if (kind === "setup") content = `<p>Set up the owner of this LOKI installation. The setup token is shown in the local console and saved in the data directory.</p>${field("token", "One-time setup token", location.hash.slice(1), "password", "required autocomplete=off")}<div class="two"><div>${field("username", "Owner username", "", "text", "required autocomplete=username minlength=3 maxlength=32")}</div><div>${field("password", "Owner password", "", "password", "required autocomplete=new-password minlength=10 maxlength=128")}</div></div>${field("name", "Server name", "LOKI Server", "text", "required maxlength=80")}${field("public_url", "Public LOKI URL", session.public_url, "url", "required")}${field("data_dir", "Data directory (optional; blank uses the current location)", "", "text", "placeholder=F:\\Servers\\LOKI")}${check("startup", "Start LOKI when I sign in to this computer", true)}<p class="help">Use your HTTPS domain for public hosting. Local development may use http://127.0.0.1:7070. Configure Caddy and automatic startup after signing in.</p>`;
  if (kind === "login") content = `<p>${app ? `Sign in with your ${esc(session.name || "LOKI")} account to continue to <strong>${esc(app)}</strong>.` : "One account for the services in your universe."}</p>${field("username", "Username or email", "", "text", "required autocomplete=username")}${field("password", "Password", "", "password", "required autocomplete=current-password")}${field("code", "Authentication or recovery code (if enabled)", "", "text", "autocomplete=one-time-code inputmode=numeric")}<p class="help">Forgot your password? Ask the server owner for a one-time reset link.</p>`;
  if (kind === "register") content = session.registration === "closed" ? "<p>Accounts are created by this server’s owner. Ask them for an activation link.</p>" : `<p>${session.registration === "approval" ? "The owner will approve your account before you can access services." : "Create an account on this LOKI server."}</p>${field("username", "Username", "", "text", "required autocomplete=username minlength=3 maxlength=32")}${field("display_name", "Display name")}${field("email", "Email (optional, unverified)", "", "email")}${field("password", "Password", "", "password", "required autocomplete=new-password minlength=10 maxlength=128")}`;
  if (kind === "reset") content = `<p>Set a password using the single-use link your server owner gave you.</p>${field("token", "Reset token", location.hash.slice(1), "password", "required autocomplete=off")}${field("password", "New password", "", "password", "required minlength=10 maxlength=128 autocomplete=new-password")}`;
  // Tokens arrive in the URL fragment; drop them from history once they are in the form.
  if (location.hash) history.replaceState(null, "", location.pathname + location.search);
  let notice = "";
  if (kind === "login") {
    try {
      const message = sessionStorage.getItem("loki-setup-notice");
      if (message) { notice = `<p class="notice" role="status">${esc(message)}</p>`; sessionStorage.removeItem("loki-setup-notice"); }
    } catch {}
  }
  const closed = kind === "register" && session.registration === "closed";
  const footer = kind === "login" ? `<a href="/register">${session.registration === "closed" ? "Account information" : "Create a LOKI account"}</a>` : kind !== "setup" ? '<a href="/login">Back to sign in</a>' : "";
  return `<section class="card auth-card${kind === "setup" ? " wide" : ""}">${sigil}<div class="eyebrow">${esc(session.name || "LOKI Server")}</div><h1>${app ? `Continue to ${esc(app)}` : titles[kind]}</h1>${notice}<form data-form="${kind}">${content}${closed ? "" : `<div class="actions"><button class="full">${submits[kind]}</button></div>`}</form>${footer ? `<hr class="divider"><div class="auth-footer">${footer}</div>` : ""}</section>`;
}
