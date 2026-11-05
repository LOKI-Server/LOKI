// The signed-in person's account, and the owner's server settings.
import { ago, enc, esc, modal, store } from "../lib/core.js";
import { icon } from "../lib/icons.js";
import { button, check, field, heading, linkButton, tag } from "../lib/ui.js";
import { startupControl } from "./auth.js";
import { avatar } from "./people.js";

export function passwordProof() {
  return `${field("current_password", "Current password", "", "password", "required autocomplete=current-password")}${store.state.mfa ? field("code", "Authentication or recovery code", "", "text", "required autocomplete=one-time-code inputmode=numeric") : ""}`;
}

const BIO_CHARS = 300;

function bioForm(bio) {
  const left = BIO_CHARS - [...bio].length;
  return `<form data-form="bio"><label for="bio">Bio</label><textarea id="bio" class="bio-input" name="bio" rows="4" maxlength="${BIO_CHARS}" data-count="bio-count" placeholder="A few words about yourself">${esc(bio)}</textarea><p class="help"><span id="bio-count">${left}</span> characters left. Shown on your profile to this server’s owner.</p><div class="actions"><button class="subtle">Save bio</button></div></form>`;
}

export function accountPage() {
  const u = store.session.user, state = store.state;
  const sessions = state.sessions.map(s => `<div class="session-row" data-key="${esc(s.id)}"><span class="session-icon" aria-hidden="true">${icon("globe")}</span><div><strong>${esc(s.label)}</strong>${s.current ? tag("This browser") : ""}<small>Signed in ${ago(s.created)}</small></div>${s.current ? "" : button("Revoke", "session-revoke", s.id, "", "ghost")}</div>`).join("");
  const devices = (state.devices || []).map(d => `<div class="session-row" data-key="${esc(d.id)}"><span class="session-icon" aria-hidden="true">${icon("server")}</span><div><strong>${esc(d.name)}</strong>${d.platform ? tag(d.platform, "dim") : ""}<small>Last seen ${ago(d.last_seen)} · signed in ${ago(d.created)}</small></div><div class="actions">${button("Rename", "device-rename", d.id, "", "ghost", "edit")}${button("Sign out", "device-revoke", d.id, "", "ghost", "logout")}</div></div>`).join("");
  return heading("Your LOKI account", "One identity, across your services.", "", "account")
    + `<div class="split"><div class="stack">`
    + `<section class="card"><div class="profile-head">${avatar(u)}<div><h2>${esc(u.display_name || u.username)}</h2><small>@${esc(u.username)}${u.owner ? " · Owner" : ""}</small></div></div>${u.bio ? `<p class="bio">${esc(u.bio)}</p>` : ""}<form data-form="profile">${field("username", "Username", u.username, "text", "required minlength=3 maxlength=32")}${field("display_name", "Display name", u.display_name)}${field("email", "Email (optional, unverified)", u.email, "email")}${passwordProof()}<div class="actions"><button>Save profile</button></div></form><hr class="divider">${bioForm(u.bio)}<hr class="divider"><form data-form="avatar"><label for="avatar-file">Profile picture</label><input id="avatar-file" name="file" type="file" accept="image/png,image/jpeg,image/webp" required><p class="help">Up to 4 MiB. Your image is resized and saved as a safe PNG.</p><div class="actions"><button class="subtle">${icon("upload")}Update picture</button></div></form></section>`
    + `<section class="card"><h2>Change password</h2><form data-form="password">${passwordProof()}${field("password", "New password", "", "password", "required minlength=10 maxlength=128 autocomplete=new-password")}<div class="actions"><button>Change password & sign out everywhere</button></div></form></section>`
    + `</div><div class="stack">`
    + `<section class="card"><div class="card-header"><h2>Two-factor authentication</h2>${state.mfa ? tag("On") : tag("Off", "dim")}</div><p>${state.mfa ? "Your account is protected by an authenticator app." : "Add an authenticator app and recovery codes to protect your account."}</p>${button(state.mfa ? "Disable two-factor authentication" : "Set up two-factor authentication", state.mfa ? "mfa-disable" : "mfa-start", "", "", state.mfa ? "subtle" : "primary", "shield")}</section>`
    + `<section class="card" id="devices"><div class="card-header"><h2>Devices</h2>${button("Pair a device", "device-pair", "", "", "primary", "plus")}</div><p>One sign-in for Save sync and shared files. Each service still follows your account’s permissions.</p>${devices ? `<div class="session-list">${devices}</div>` : '<p class="help">No devices are signed in yet.</p>'}<p class="help">Server address for clients: <span class="mono">${esc(store.session.public_url)}</span></p></section>`
    + `<section class="card"><h2>Browser sessions</h2><div class="session-list">${sessions}</div><div class="actions">${button("Sign out", "logout", "", "", "subtle", "logout")}${button("Sign out everywhere, including apps", "revoke-all", "", "", "danger")}</div></section>`
    + `</div></div>`;
}

export function settingsPage() {
  const s = store.state.settings, state = store.state;
  const registration = [["approval", "Request account, then owner approval"], ["open", "Open registration"], ["closed", "Owner-created accounts only"]];
  return heading("Server settings", "Keep the branches growing, on your terms.", "", "settings")
    + `<div class="split"><section class="card"><form data-form="settings"><h2>Your server</h2>${field("name", "Server name", s.name, "text", "required maxlength=80")}${field("public_url", "Canonical public URL", s.public_url, "url", "required")}<label for="registration-mode">Account registration</label><select id="registration-mode" name="registration">${registration.map(([v, label]) => `<option value="${v}" ${s.registration === v ? "selected" : ""}>${label}</option>`).join("")}</select>${startupControl(s)}`
    + `<hr class="divider"><h2>Save sync</h2><div class="two"><div>${field("sync_quota_mb", "Default storage per person (MiB)", s.sync_quota_mb, "number", "required min=1")}</div><div>${field("sync_max_file_mb", "Largest file (MiB)", s.sync_max_file_mb, "number", "required min=1 max=1048576")}</div></div><p class="help">Storage includes each person’s version history. Override it per person in People &amp; groups.</p>`
    + `<hr class="divider"><h2>App registry</h2>${field("registry_url", "Registry address", s.registry_url, "url", 'placeholder="Off"')}<p class="help">Browse apps installs from this list, and LOKI checks it for updates to apps installed from it. Leave it empty to turn it off.</p>`
    + `<hr class="divider"><h2>Caddy</h2>${check("caddy_enabled", "Enable Caddy management", s.caddy_enabled)}<label for="caddy-mode">Management mode</label><select id="caddy-mode" name="caddy_mode"><option value="managed" ${s.caddy_mode === "managed" ? "selected" : ""}>LOKI-managed Caddy process</option><option value="existing" ${s.caddy_mode === "existing" ? "selected" : ""}>Existing Caddy instance (LOKI is sole writer)</option></select><p class="help">Managed mode starts and stops Caddy with LOKI. Existing mode connects to Caddy you already started separately.</p>${field("caddy_binary", "Caddy executable", s.caddy_binary)}<p class="help">Installed LOKI includes caddy.exe in its installation folder. Use its full path if caddy is not on PATH.</p>${field("caddy_admin", "Loopback management URL", s.caddy_admin, "url")}<details class="advanced"${s.caddy_base ? " open" : ""}><summary>Caddy base JSON (advanced)</summary><label for="caddy-base" class="visually-hidden">Caddy base JSON</label><textarea id="caddy-base" name="caddy_base" spellcheck="false">${esc(s.caddy_base)}</textarea><p class="help">Leave base JSON blank to replace existing sites with LOKI routes. Adopt existing captures and backs up sites to preserve them. Existing mode keeps the running management listener. Stop Patty or any other configuration writer before applying changes.</p></details><div class="actions">${button("Adopt existing (preview)", "caddy-adopt")}${button("Validate & preview", "caddy-preview", "", "", "subtle", "check")}<button>Save settings</button></div></form></section>`
    + `<div class="stack"><section class="card"><h2>System</h2><p class="mono path-display">${icon("server")}${esc(state.data_dir)}</p><small>Data location · ${esc(state.target)}</small><p class="help">Use an empty folder when moving or restoring. LOKI keeps the current data as a safety copy and restarts using the new location.</p><div class="actions">${button("Move data directory", "storage-move", "", "", "subtle", "folder")}${button("Restore a backup", "storage-restore", "", "", "subtle", "archive")}${button("Run diagnostics", "diagnostics", "", "", "subtle", "activity")}${button("Caddy logs", "logs", "caddy", "", "subtle", "logs")}${button("Apply routes", "caddy-apply", "", "", "subtle", "route")}</div></section>`
    + `<section class="card"><h2>Backups</h2><p>Snapshot LOKI and installed app data. Apps stop briefly while the snapshot is taken. Shared external folders are backed up separately.</p><div class="actions">${button("Create backup", "backup", "", "", "primary", "archive")}${button("Import backup", "backup-import", "", "", "subtle", "upload")}</div>${state.backups.length ? `<div class="file-list">${state.backups.map(b => `<a href="/api/backups/${enc(b)}" download>${icon("download")}<span>${esc(b)}</span></a>`).join("")}</div>` : '<p class="help">No backups yet.</p>'}<p class="help">Backup archives contain credentials and private data. Keep downloaded copies somewhere private.</p></section></div></div>`;
}

export function deviceRename(id) {
  const d = store.state.devices.find(d => d.id === id);
  modal("Rename device", `<form data-form="device-rename" data-id="${esc(id)}">${field("name", "Device name", d.name, "text", "required maxlength=80")}<div class="actions"><button>Save</button></div></form>`);
}

// Any device that signs in with a code comes here: sync clients, and apps on
// consoles, phones and desktops.
export function pairPage() {
  const code = new URLSearchParams(location.search).get("code") || "";
  return heading("Sign in a device", "Approve a code shown on a device, so it can use your account without your password.", linkButton("Back to your account", "/account", "subtle", "left"), "account")
    + `<section class="card narrow"><form data-form="device-code">${field("code", "Code shown on your device", code, "text", "required autocomplete=off autocapitalize=characters placeholder=ABCD-EFGH maxlength=12")}<p class="help">Codes expire after ten minutes. Only approve devices you are setting up yourself.</p><div class="actions"><button>${icon("check")}Continue</button></div></form></section>`;
}
export function pairDialog(device) {
  const sync = device.kind === "sync" || device.kind === "account";
  const reach = sync ? "use Save sync and download shared files your account can access until you sign it out from Account → Devices" : `use ${esc(device.service)} as you until you sign out of it there`;
  modal(sync ? "Approve this device?" : `Sign in to ${device.service}?`, `<div class="session-row"><span class="session-icon" aria-hidden="true">${icon("server")}</span><div><strong>${esc(device.name)}</strong>${device.platform ? tag(device.platform, "dim") : ""}<small>Code ${esc(device.code)}${sync ? "" : ` · ${esc(device.service)}`}</small></div></div><p>This device will be able to ${reach}.</p><div class="actions">${button("Approve", "device-approve", "", `data-code="${esc(device.code)}" data-kind="${esc(device.kind)}"`, "primary", "check")}${button("Decline", "device-deny", "", `data-code="${esc(device.code)}" data-kind="${esc(device.kind)}"`, "danger", "close")}</div>`);
}
