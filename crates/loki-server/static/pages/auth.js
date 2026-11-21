// Setup, sign-in, registration, password reset, and the Windows startup review.
import { $, api, modal, store } from "../lib/core.js";
import { html, raw } from "../lib/html.js";
import { Button, Checkbox, Field, FieldRow, Form, FormFooter, Help, LinkButton, Notice, PageHeader, Submit } from "../lib/ui.js";

const futhark = "ᚠ ᚢ ᚦ ᚨ ᚱ ᚲ ᚷ ᚹ ᚺ ᚾ ᛁ ᛃ ᛇ ᛈ ᛉ ᛊ ᛏ ᛒ ᛖ ᛗ ᛚ ᛜ ᛞ ᛟ";
// The twenty-four runes of the Elder Futhark, circling the Algiz rune of protection.
const sigil = raw(`<div class="sigil" aria-hidden="true"><svg class="rune-ring" viewBox="0 0 200 200"><defs><path id="ring-path" d="M100 100m-73 0a73 73 0 1 1 146 0a73 73 0 1 1-146 0"/></defs><circle cx="100" cy="100" r="94"/><circle cx="100" cy="100" r="62"/><text><textPath href="#ring-path" textLength="455">${futhark}</textPath></text></svg><span class="rune">ᛉ</span></div>`);

export function setupLogin(message) {
  try { sessionStorage.setItem("loki-setup-notice", message); } catch {}
  location.assign("/login");
}

export function confirmStartupReplacement(details) {
  return new Promise(resolve => {
    const dialog = $("#dialog");
    let settled = false;
    const dismissed = () => { if (!settled) { settled = true; resolve(null); } };
    modal("Replace the old startup entry?", html`<p>Windows is set to start another copy of LOKI when you sign in.</p><dl class="paths"><dt>Currently starts</dt><dd class="mono">${details.existing_executable}</dd><dt>Start this copy instead</dt><dd class="mono">${details.current_executable}</dd><dt>Data folder used by this copy</dt><dd class="mono">${details.data_directory}</dd></dl>${Help("Replacing the startup entry changes which executable runs at sign-in. Your data stays in the folder shown above. The old entry is replaced so both copies do not start.")}${FormFooter([Button({ label: "Cancel", attrs: { id: "startup-cancel" } }), Button({ label: "Replace startup entry", kind: "primary", attrs: { id: "startup-replace" } })], { divider: false })}`);
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
  const help = other ? html`Windows currently starts another LOKI copy: <span class="mono">${startup.registered_executable}</span>. Turn this on and save to review replacing it with this copy.` : startup?.error || "";
  return Checkbox({ name: "startup", label: "Start LOKI when I sign in to this computer", checked: settings.startup && !other, help });
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
    $("#main").innerHTML = String(PageHeader({ title: "Your universe is ready.", description: "LOKI is restarting in the selected data directory. The sign-in page will open shortly." }));
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
  const { session } = store, server = session.name || "LOKI";
  if (!session.user) return authCard(server, html`<h1>You’re signed out.</h1><p>You’re signed out of ${server} and its apps on this device.</p>${LinkButton({ label: "Sign in", href: "/login", kind: "primary", block: true })}`);
  return authCard(server, html`<h1>Sign out of ${server}?</h1><p>This also signs you out of the apps you opened from this device.</p>${Button({ label: "Sign out", action: "logout", kind: "primary", icon: "logout", block: true })}`, html`<a href="/">Stay signed in</a>`);
}

const authCard = (eyebrow, body, footer = "", wide = false) => html`<section class="${wide ? "auth-card wide" : "auth-card"}">${sigil}<div class="eyebrow">${eyebrow}</div>${body}${footer ? html`<div class="auth-footer">${footer}</div>` : ""}</section>`;

function fields(kind, session, app) {
  if (kind === "setup") return html`<p>Set up the owner of this LOKI installation. The setup token is shown in the local console and saved in the data directory.</p>${Field({ name: "token", label: "One-time setup token", value: location.hash.slice(1), type: "password", required: true, autocomplete: "off" })}${FieldRow(Field({ name: "username", label: "Owner username", required: true, autocomplete: "username", minlength: 3, maxlength: 32 }), Field({ name: "password", label: "Owner password", type: "password", required: true, autocomplete: "new-password", minlength: 10, maxlength: 128 }))}${Field({ name: "name", label: "Server name", value: "LOKI Server", required: true, maxlength: 80 })}${Field({ name: "public_url", label: "Public LOKI URL", value: session.public_url, type: "url", required: true, help: "Use your HTTPS domain for public hosting. Local development may use http://127.0.0.1:7070." })}${Field({ name: "data_dir", label: "Data directory (optional)", placeholder: "F:\\Servers\\LOKI", help: "Leave blank to keep the current location." })}${Checkbox({ name: "startup", label: "Start LOKI when I sign in to this computer", checked: true, help: "You can set up Caddy and change this after signing in." })}`;
  if (kind === "login") return html`<p>${app ? html`Sign in with your ${session.name || "LOKI"} account to continue to <strong>${app}</strong>.` : "One account for the services in your universe."}</p>${Field({ name: "username", label: "Username or email", required: true, autocomplete: "username" })}${Field({ name: "password", label: "Password", type: "password", required: true, autocomplete: "current-password" })}${Field({ name: "code", label: "Authentication or recovery code (if enabled)", autocomplete: "one-time-code", inputmode: "numeric", help: "Forgot your password? Ask the server owner for a one-time reset link." })}`;
  if (kind === "register") return session.registration === "closed" ? html`<p>Accounts are created by this server’s owner. Ask them for an activation link.</p>` : html`<p>${session.registration === "approval" ? "The owner will approve your account before you can access services." : "Create an account on this LOKI server."}</p>${Field({ name: "username", label: "Username", required: true, autocomplete: "username", minlength: 3, maxlength: 32 })}${Field({ name: "display_name", label: "Display name" })}${Field({ name: "email", label: "Email (optional, unverified)", type: "email" })}${Field({ name: "password", label: "Password", type: "password", required: true, autocomplete: "new-password", minlength: 10, maxlength: 128 })}`;
  return html`<p>Set a password using the single-use link your server owner gave you.</p>${Field({ name: "token", label: "Reset token", value: location.hash.slice(1), type: "password", required: true, autocomplete: "off" })}${Field({ name: "password", label: "New password", type: "password", required: true, minlength: 10, maxlength: 128, autocomplete: "new-password" })}`;
}

export function authPage(kind) {
  const session = store.session;
  const app = kind === "login" ? signingInTo() : "";
  const content = fields(kind, session, app);
  // Tokens arrive in the URL fragment; drop them from history once they are in the form.
  if (location.hash) history.replaceState(null, "", location.pathname + location.search);
  let notice = "";
  if (kind === "login") {
    try {
      const message = sessionStorage.getItem("loki-setup-notice");
      if (message) { notice = Notice({ tone: "ok", body: message }); sessionStorage.removeItem("loki-setup-notice"); }
    } catch {}
  }
  const closed = kind === "register" && session.registration === "closed";
  const footer = kind === "login" ? html`<a href="/register">${session.registration === "closed" ? "Account information" : "Create a LOKI account"}</a>` : kind !== "setup" ? html`<a href="/login">Back to sign in</a>` : "";
  return authCard(session.name || "LOKI Server", html`<h1>${app ? `Continue to ${app}` : titles[kind]}</h1>${notice}${Form({ name: kind, body: html`${content}${closed ? "" : FormFooter(Submit({ label: submits[kind], block: true }), { divider: false })}` })}`, footer, kind === "setup");
}
