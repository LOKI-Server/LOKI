// The signed-in person's account, and the owner's server settings.
import { ago, api, date, enc, modal, store } from "../lib/core.js";
import { attrs, html } from "../lib/html.js";
import { icon } from "../lib/icons.js";
import { Badge, Button, ButtonGroup, Card, Checkbox, Field, FieldRow, FileInput, Form, FormFooter, Help, IconLink, LinkButton, List, Notice, PageHeader, Row, Select, Split, Submit, TextArea } from "../lib/ui.js";
import { startupControl } from "./auth.js";
import { avatar } from "./people.js";

export function passwordProof() {
  return html`${Field({ name: "current_password", label: "Current password", type: "password", required: true, autocomplete: "current-password" })}${store.state.mfa ? Field({ name: "code", label: "Authentication or recovery code", required: true, autocomplete: "one-time-code", inputmode: "numeric" }) : ""}`;
}

const BIO_CHARS = 300;

export async function exportPage() {
  const { folders, saves } = await api("/api/personal");
  const option = (name, value, label) => html`<label class="check"><input${attrs({ type: "checkbox", name, value })}><span>${label}</span></label>`;
  return html`${PageHeader({ title: "Export my data", description: "Choose the data to include in your ZIP file.", actions: LinkButton({ label: "My account", href: "/account", icon: "left" }), realm: "archive" })}${Card({ className: "narrow", body: html`<form action="/api/personal/export" method="get">${folders.length ? html`<fieldset><legend>Personal app files</legend><div class="check-list">${folders.map(id => option("app", id, id))}</div></fieldset>` : ""}${saves.length ? html`<fieldset><legend>Current Save sync files</legend><div class="check-list">${saves.map(([id, name]) => option("save", id, name))}</div></fieldset>` : ""}<fieldset><legend>Account</legend><div class="check-list">${option("profile", "1", "Account profile")}</div></fieldset>${FormFooter(Submit({ label: "Download ZIP", icon: "download" }))}</form>` })}`;
}

function bioForm(bio) {
  const left = BIO_CHARS - [...bio].length;
  return Form({ name: "bio", body: html`${TextArea({ name: "bio", label: "Bio", value: bio, rows: 4, maxlength: BIO_CHARS, placeholder: "A few words about yourself", attrs: { "data-count": "bio-count" } })}<p class="help"><span id="bio-count">${left}</span> characters left. Shown on your profile to this server’s owner.</p>${FormFooter(Submit({ label: "Save bio", kind: "secondary" }))}` });
}

export async function vcsCredentialsPage() {
  const tokens = await api("/api/account/vcs-tokens");
  const rows = tokens.map(token => Row({ key: token.id, icon: "key", title: token.label, badges: Badge(token.write ? "Read and write" : "Read", "neutral"), meta: `Expires ${date(token.expires)}`, actions: Button({ label: "Revoke", action: "vcs-revoke-token", id: token.id, kind: "danger", size: "sm" }) }));
  return html`${PageHeader({ title: "Skald tokens", description: "Tokens let the skald command line reach repositories on this server.", actions: [LinkButton({ label: "Repositories", href: "/repositories", icon: "repo" }), Button({ label: "Create token", action: "vcs-new-token", kind: "primary", icon: "plus" })], realm: "repos" })}${Card({ className: "narrow", flush: true, title: "Your tokens", description: "Set SKALD_TOKEN in the terminal where you run skald. Use a separate token for each computer.", body: List(rows, { empty: "No tokens yet." }) })}`;
}

export function vcsTokenDialog() {
  modal("Create Skald token", Form({ name: "vcs-token", body: html`${Field({ name: "label", label: "Name", required: true, maxlength: 80, placeholder: "Development PC" })}${Select({ name: "scope", label: "Access", options: [["write", "Read and write"], ["read", "Read"]] })}${passwordProof()}${Help("This token expires after 90 days.")}${FormFooter(Submit({ label: "Create token" }))}` }));
}

export function accountPage() {
  const u = store.session.user, state = store.state;
  const sessions = state.sessions.map(s => Row({ key: s.id, icon: "globe", title: s.label, badges: s.current ? Badge("This browser") : "", meta: html`Signed in ${ago(s.created)}`, actions: s.current ? "" : Button({ label: "Revoke", action: "session-revoke", id: s.id, kind: "ghost", size: "sm" }) }));
  const devices = (state.devices || []).map(d => Row({
    key: d.id, icon: "server", title: d.name,
    badges: [d.platform ? Badge(d.platform, "neutral") : "", d.scope ? Badge(d.scope === "owner" ? "Owner API" : "Repository API", "info") : ""],
    meta: html`Last seen ${ago(d.last_seen)} · signed in ${ago(d.created)}${d.expires ? ` · expires ${date(d.expires)}` : ""}`,
    actions: [Button({ label: "Rename", action: "device-rename", id: d.id, kind: "ghost", icon: "edit", size: "sm" }), Button({ label: "Sign out", action: "device-revoke", id: d.id, kind: "ghost", icon: "logout", size: "sm" })],
  }));
  const profile = Card({ body: html`<div class="profile-head">${avatar(u, "xl")}<div><h2>${u.display_name || u.username}</h2><small class="muted">@${u.username}${u.owner ? " · Owner" : ""}</small></div></div>${u.bio ? html`<p class="bio">${u.bio}</p>` : ""}${Form({ name: "profile", body: html`${Field({ name: "username", label: "Username", value: u.username, required: true, minlength: 3, maxlength: 32 })}${Field({ name: "display_name", label: "Display name", value: u.display_name })}${Field({ name: "email", label: "Email (optional, unverified)", value: u.email, type: "email" })}${passwordProof()}${FormFooter(Submit({ label: "Save profile" }))}` })}<hr class="divider">${bioForm(u.bio || "")}<hr class="divider">${Form({ name: "avatar", body: html`${FileInput({ name: "file", label: "Profile picture", accept: "image/png,image/jpeg,image/webp", help: "Up to 4 MiB. Your image is resized and saved as a safe PNG." })}${FormFooter(Submit({ label: "Update picture", kind: "secondary", icon: "upload" }))}` })}` });
  const password = Card({ title: "Change password", body: Form({ name: "password", body: html`${passwordProof()}${Field({ name: "password", label: "New password", type: "password", required: true, minlength: 10, maxlength: 128, autocomplete: "new-password" })}${FormFooter(Submit({ label: "Change password & sign out everywhere" }))}` }) });
  const mfa = Card({ title: "Two-factor authentication", actions: state.mfa ? Badge("On") : Badge("Off", "neutral"), body: html`<p>${state.mfa ? "Your account is protected by an authenticator app." : "Add an authenticator app and recovery codes to protect your account."}</p>${Button({ label: state.mfa ? "Turn off two-factor authentication" : "Set up two-factor authentication", action: state.mfa ? "mfa-disable" : "mfa-start", kind: state.mfa ? "secondary" : "primary", icon: "shield", size: "sm" })}` });
  const deviceCard = Card({ id: "devices", flush: true, title: "Devices", description: "Save sync, shared-file, and API sign-ins. Each follows your account’s permissions.", actions: Button({ label: "Pair a device", action: "device-pair", icon: "plus", size: "sm" }), body: html`${List(devices, { empty: "No devices are signed in yet." })}<p class="card-foot help">Server address for clients: <span class="mono">${store.session.public_url}</span></p>` });
  const skald = Card({ title: "Skald tokens", body: html`<p>Tokens let the skald command line reach your repositories from your computer.</p>${LinkButton({ label: "Manage tokens", href: "/account/vcs-credentials", icon: "key", size: "sm" })}` });
  const browsers = Card({ flush: true, title: "Browser sessions", body: html`${List(sessions)}<div class="card-foot">${ButtonGroup(Button({ label: "Sign out", action: "logout", icon: "logout", size: "sm" }), Button({ label: "Sign out everywhere, including apps", action: "revoke-all", kind: "danger", size: "sm" }))}</div>` });
  return html`${PageHeader({ title: "Your LOKI account", description: "One identity, across your services.", actions: LinkButton({ label: "Export my data", href: "/account/export", icon: "download" }), realm: "account" })}${Split([profile, password], [mfa, deviceCard, skald, browsers])}`;
}

// One labelled group of settings: what it is on the left, the controls on the right.
const group = (id, title, text, body) => html`<section class="setting-group" id="${id}"><div class="setting-intro"><h2>${title}</h2><p>${text}</p></div>${Card({ body })}</section>`;
const mib = n => n >= 1024 && n % 1024 === 0 ? `${n / 1024} GiB` : `${n} MiB`;

// Backups are named loki-<unix seconds>-<id>.zip.
function backupRow(name) {
  const when = /^loki-(\d{9,})-/.exec(name);
  return Row({ key: name, icon: "archive", title: when ? ago(Number(when[1])) : name, meta: html`<span class="mono">${name}</span>`, actions: IconLink({ icon: "download", label: `Download ${name}`, href: `/api/backups/${enc(name)}`, download: true, size: "sm" }) });
}

export function settingsPage() {
  const s = store.state.settings, state = store.state;
  const registration = [["approval", "Request account, then owner approval"], ["open", "Open registration"], ["closed", "Owner-created accounts only"]];
  const sync = state.resources.find(r => r.kind === "sync");
  const sections = [["general", "General"], ["features", "Built-in features"], ["registry", "App registry"], ["caddy", "Reverse proxy"], ["backups", "Backups"], ["data", "Data & diagnostics"]];
  const save = FormFooter(Submit({ label: "Save changes" }));
  return html`${PageHeader({ title: "Server settings", description: "Keep the branches growing, on your terms.", realm: "settings" })}<div class="settings-layout"><nav class="settings-nav" aria-label="Settings sections">${sections.map(([id, label]) => html`<a href="${`#${id}`}">${label}</a>`)}</nav><div class="settings-body">${[
    group("general", "General", "How this server introduces itself, and who may join.",
      Form({ name: "settings", body: html`${Field({ name: "name", label: "Server name", value: s.name, required: true, maxlength: 80, help: "Shown in the browser tab and at the top of every page." })}${Field({ name: "public_url", label: "Canonical public URL", value: s.public_url, type: "url", required: true, help: "The address people and devices use to reach LOKI." })}${Select({ name: "registration", label: "Account registration", options: registration, value: s.registration })}${startupControl(s)}${save}` })),
    group("features", "Built-in features", "Limits and access for Save sync and repositories live with each feature.",
      List([sync ? Row({ icon: "sync", title: "Save sync", meta: `${mib(s.sync_quota_mb)} per person · files up to ${mib(s.sync_max_file_mb)}`, actions: Button({ label: "Manage", action: "sync-manage", id: sync.id, icon: "settings", size: "sm" }) }) : "", Row({ icon: "repo", title: "Repositories", meta: `${mib(s.vcs_repo_mib)} per repository · ${mib(s.vcs_total_mib)} in total`, actions: Button({ label: "Manage", action: "vcs-manage", icon: "settings", size: "sm" }) })], { className: "list-plain" })),
    group("registry", "App registry", "Where Browse apps finds apps, and where LOKI looks for their updates.",
      Form({ name: "settings", body: html`${Field({ name: "registry_url", label: "Registry address", value: s.registry_url, type: "url", placeholder: "Off", help: "Leave it empty to turn the registry off." })}${save}` })),
    group("caddy", "Reverse proxy", "LOKI can configure Caddy to serve your apps and connected services on their own hostnames.",
      Form({ name: "settings", body: html`${Checkbox({ name: "caddy_enabled", label: "Let LOKI manage Caddy", checked: s.caddy_enabled })}${Select({ name: "caddy_mode", label: "Mode", options: [["managed", "LOKI starts and stops its own Caddy"], ["existing", "Use a Caddy that is already running"]], value: s.caddy_mode })}${FieldRow(Field({ name: "caddy_binary", label: "Caddy executable", value: s.caddy_binary }), Field({ name: "caddy_admin", label: "Management URL (loopback)", value: s.caddy_admin, type: "url" }))}${Help("The installer puts caddy.exe beside LOKI. Use a full path if caddy is not on PATH. To change these, turn management off and save first.")}<details class="disclosure"${attrs({ open: !!s.caddy_base })}><summary>Base configuration (advanced)</summary>${TextArea({ name: "caddy_base", label: "Caddy base JSON", value: s.caddy_base, mono: true, hiddenLabel: true, help: "Blank replaces existing sites with LOKI's routes. Adopt existing backs up the running configuration and loads it here so its sites are kept. Stop any other tool that writes Caddy's configuration first." })}${Button({ label: "Adopt existing configuration", action: "caddy-adopt", size: "sm" })}</details>${FormFooter([Button({ label: "Logs", action: "logs", id: "caddy", kind: "ghost", icon: "logs" }), Button({ label: "Apply routes", action: "caddy-apply", kind: "ghost", icon: "route" }), Button({ label: "Validate & preview", action: "caddy-preview", icon: "check" }), Submit({ label: "Save changes" })])}` })),
    group("backups", "Backups", "Snapshots of LOKI and installed app data. Apps pause briefly while one is taken. Shared folders outside LOKI are not included.",
      html`${ButtonGroup(Button({ label: "Create backup", action: "backup", kind: "primary", icon: "archive" }), Button({ label: "Import", action: "backup-import", icon: "upload" }))}${List(state.backups.map(backupRow), { className: "list-plain", empty: "No backups yet." })}${Notice({ tone: "info", icon: "lock", body: "Backups contain credentials and private data. Keep downloaded copies somewhere private." })}`),
    group("data", "Data & diagnostics", "Where LOKI keeps its data on this computer.",
      html`<p class="path-display mono">${icon("database")}${state.data_dir}</p><p class="help">${state.target} · LOKI ${state.loki_version}</p><p>Moving or restoring needs an empty folder. LOKI keeps the current data as a safety copy, then restarts from the new location.</p>${ButtonGroup(Button({ label: "Run diagnostics", action: "diagnostics", icon: "activity" }), Button({ label: "Move data", action: "storage-move", icon: "folder" }), Button({ label: "Restore a backup", action: "storage-restore", icon: "refresh" }))}`),
  ]}</div></div>`;
}

export function deviceRename(id) {
  const d = store.state.devices.find(d => d.id === id);
  modal("Rename device", Form({ name: "device-rename", id, body: html`${Field({ name: "name", label: "Device name", value: d.name, required: true, maxlength: 80 })}${FormFooter(Submit({ label: "Save" }))}` }));
}

// Any device that signs in with a code comes here: sync clients, and apps on
// consoles, phones and desktops.
export function pairPage() {
  const code = new URLSearchParams(location.search).get("code") || "";
  return html`${PageHeader({ title: "Sign in a device", description: "Approve a code shown on a device, so it can use your account without your password.", actions: LinkButton({ label: "Back to your account", href: "/account", icon: "left" }), realm: "account" })}${Card({ className: "narrow", body: Form({ name: "device-code", body: html`${Field({ name: "code", label: "Code shown on your device", value: code, required: true, autocomplete: "off", placeholder: "ABCD-EFGH", maxlength: 12, attrs: { autocapitalize: "characters" }, help: "Codes expire after ten minutes. Only approve devices you are setting up yourself." })}${FormFooter(Submit({ label: "Continue", icon: "check" }))}` }) })}`;
}

export function pairDialog(device) {
  const sync = device.kind === "sync" || device.kind === "account";
  const reach = device.kind === "api-owner"
    ? "manage repositories and administer this server, including users and groups, for up to 90 days"
    : device.kind === "api-repositories"
    ? "create, read, push, and manage repositories, and use permitted Save sync and shared files, for up to 90 days"
    : sync ? "use Save sync and download shared files your account can access until you sign it out from Account → Devices" : `use ${device.service} as you until you sign out of it there`;
  const decline = Button({ label: "Decline", action: "device-deny", kind: "danger", icon: "close", data: { code: device.code, kind: device.kind } });
  if (device.kind === "api-owner") {
    modal("Approve LOKI owner API access?", html`<p>Allow <strong>${device.name}</strong> to ${reach}.</p>${store.session.user.owner ? Form({ name: "device-owner-approve", data: { code: device.code }, body: html`${passwordProof()}${FormFooter([decline, Submit({ label: "Approve owner access" })])}` }) : FormFooter(decline)}`);
    return;
  }
  modal(sync ? "Approve this device?" : `Sign in to ${device.service}?`, html`${List(Row({ icon: "server", title: device.name, badges: device.platform ? Badge(device.platform, "neutral") : "", meta: `Code ${device.code}${sync ? "" : ` · ${device.service}`}` }))}<p>This device will be able to ${reach}.</p>${FormFooter([decline, Button({ label: "Approve", action: "device-approve", kind: "primary", icon: "check", data: { code: device.code, kind: device.kind } })], { divider: false })}`);
}
