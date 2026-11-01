// Owner dialogs for managing applications, shares, routes, and who can reach them.
import { api, esc, modal, resource, store } from "../lib/core.js";
import { icon } from "../lib/icons.js";
import { button, check, field, tag } from "../lib/ui.js";
import { resourceStatus } from "./services.js";

export const subjects = () => [
  ...store.state.groups.map(g => ({ key: `group:${g.id}`, label: `Group: ${g.name}` })),
  ...store.state.users.map(u => ({ key: `user:${u.id}`, label: `Person: ${u.username}` })),
];
export const roles = r => [...new Set(["access", ...(r.kind === "app" ? ["admin", ...r.manifest.roles] : [])])];

export function accessPanel(r) {
  const grants = store.state.grants.filter(g => g.resource === r.id);
  const audiences = { restricted: "Selected people and groups", members: "All approved members", ...(r.kind === "sync" ? {} : { public: "Public" }) };
  return `<form data-form="access" data-id="${esc(r.id)}"><label for="access-visibility">Audience</label><select id="access-visibility" name="visibility">${Object.entries(audiences).map(([v, label]) => `<option ${r.visibility === v ? "selected" : ""} value="${v}">${label}</option>`).join("")}</select>${r.kind === "app" ? '<p class="help">Public visibility makes the service discoverable, but this app may still require a LOKI sign-in for its features.</p>' : ""}${r.kind === "route" && !r.gated ? '<p class="help">Without a browser sign-in gate, these permissions control the service card in LOKI. The upstream service controls sign-in.</p>' : ""}<label>Permissions</label><div id="grants">${grants.map(g => grantRow(subjects(), roles(r), `${g.subject_kind}:${g.subject}`, g.role)).join("")}</div>${button("Add permission", "grant-add", r.id, "", "ghost", "plus")}<div class="actions"><button>Save access</button></div></form>`;
}

export function grantRow(subjects, roles, selected = "", role = "access") {
  return `<div class="grant-row"><select class="grant-subject" aria-label="Person or group">${subjects.map(s => `<option value="${esc(s.key)}" ${s.key === selected ? "selected" : ""}>${esc(s.label)}</option>`).join("")}</select><select class="grant-role" aria-label="Role">${roles.map(r => `<option ${r === role ? "selected" : ""}>${esc(r)}</option>`).join("")}</select>${button("Remove", "grant-remove", "", 'aria-label="Remove permission"', "icon-button", "trash")}</div>`;
}

const section = (title, body) => `<section class="dialog-section"><h3>${esc(title)}</h3>${body}</section>`;

export function syncManage(id) {
  const r = resource(id);
  modal(r.name, `<p>Who may pair devices and keep files in sync. Each person only ever sees their own folders. Set storage limits in Server settings and per person in People &amp; groups.</p>` + section("Who can use sync", accessPanel(r)));
}

// Apps with live connections, such as game servers, can also listen on the LAN.
function realtimeSettings(r) {
  if (!r.manifest.capabilities?.includes("realtime")) return "";
  const path = r.manifest.limits?.realtime?.path || "/multiplayer";
  return `<div class="notice">Anyone who can reach this app can connect to its live endpoint (<span class="mono">${esc(path)}</span>) without a LOKI account. The app decides who may join.</div>${field("lan_bind", "LAN address (optional)", r.lan_bind || "", "text", "placeholder=0.0.0.0:4767")}<p class="help">Also accept connections on this address, so devices on your network can connect by IP. Only the live endpoint and health check answer there. Allow the port in your firewall.</p>`;
}

export function appManage(id) {
  const r = resource(id);
  const operation = (label, op, kind = "subtle", iconName = "") => button(label, "app-operation", id, `data-operation="${op}"`, kind, iconName);
  modal(r.name, `<p>${esc(r.manifest.description)}</p><div class="tags">${tag(`v${r.manifest.version}`)}${tag(r.manifest.runtime === "wasi" ? "WASI component" : "Native service")}${resourceStatus(r)}</div>${r.error ? `<p class="notice error">${esc(r.error)}</p>` : ""}<div class="actions">${operation(r.enabled ? "Disable" : "Enable", r.enabled ? "disable" : "enable", "primary", "power")}${r.enabled ? operation("Restart", "restart", "subtle", "refresh") : ""}${r.manifest.panel && r.ready ? button("Open panel", "app-panel", id, "", "subtle", "apps") : ""}${button("Logs", "logs", id, "", "subtle", "logs")}${r.previous ? operation("Roll back version & data", "rollback", "subtle", "left") : ""}${operation("Uninstall (keep data)", "uninstall", "danger", "trash")}</div>`
    + section("Settings", `<form data-form="app-config" data-id="${esc(id)}">${field("hostname", "Public hostname (optional)", r.hostname, "text", "placeholder=bigshot.example.com")}<p class="help">Leave blank for loopback access. Your domain needs DNS and a configured Caddy.</p>${realtimeSettings(r)}${r.manifest.fields.map(f => f.kind === "bool" ? check(f.key, f.label, r.config[f.key] ?? f.default) : field(f.key, f.label, r.config[f.key] ?? f.default ?? "", f.kind === "secret" ? "password" : f.kind === "number" ? "number" : "text", f.required ? "required" : "")).join("")}<div class="actions"><button>Save application settings</button></div></form>`)
    + section("Who can use this app", accessPanel(r)));
}

export function shareForm(r = {}) {
  const excluded = Array.isArray(r.excluded_extensions) ? r.excluded_extensions.map(e => "." + e).join(", ") : (r.excluded_extensions || "");
  return `<form data-form="share" data-id="${esc(r.id || "")}">${field("name", "Share name", r.name || "", "text", "required")}<div class="input-with-button">${field("root", "Folder on this server", r.root || "", "text", "required")}${button("Browse", "folder-picker", r.id || "", "", "subtle", "folder")}</div>${field("excluded_extensions", "Ignore file extensions (optional)", excluded, "text", 'maxlength="4096" placeholder=".tmp, .bak, .psd, .tar.gz"')}<p class="help">Separate extensions with commas or spaces. Matching ignores case and applies in every folder. These files are hidden from listings and cannot be previewed or downloaded through this share. Folders are kept.</p><div class="actions"><button>${r.id ? "Save share" : "Create share"}</button>${r.id ? button("Remove share", "share-delete", r.id, "", "danger", "trash") : ""}</div>${r.id ? "" : '<p class="help">New shares are restricted to you until you grant access.</p>'}</form>`;
}
export function shareManage(id, draft = {}) {
  const r = resource(id);
  modal(r.name, shareForm({ ...r, ...draft, id }) + section("Who can browse this share", accessPanel(r)));
}

export function routeForm(r = {}) {
  const adapters = { none: "No OS service", windows: "Windows service", systemd: "Linux systemd", brew: "macOS Homebrew" };
  return `<form data-form="route" data-id="${esc(r.id || "")}">${field("name", "Service name", r.name || "")}${field("hostname", "Public hostname", r.hostname || "", "text", "placeholder=jellyfin.example.com required")}${field("upstream", "Upstream origin", r.upstream || "http://127.0.0.1:8096", "url", "required")}${check("gated", "Require LOKI browser sign-in before forwarding", r.gated)}<p class="help">Leave the browser gate off for Jellyfin and services used by native clients. They keep their own login.</p><div class="two"><div><label for="service-kind">OS service adapter (optional)</label><select id="service-kind" name="service_kind">${Object.entries(adapters).map(([k, label]) => `<option value="${k}" ${r.service_kind === k ? "selected" : ""}>${label}</option>`).join("")}</select></div><div>${field("service", "OS service name", r.service || "")}</div></div><div class="actions"><button>Save route</button></div></form>`;
}
export function routeManage(id) {
  const r = resource(id);
  const operation = (label, op, kind, iconName) => button(label, "route-operation", id, `data-operation="${op}"`, kind, iconName);
  modal(r.name, `<div class="tags">${resourceStatus(r)}${r.gated ? tag("Sign-in gate") : ""}</div><div class="actions">${operation(r.enabled ? "Disable route" : "Enable route", r.enabled ? "disable" : "enable", "primary", "power")}${operation("Delete route", "delete", "danger", "trash")}</div>`
    + (r.service ? section("OS service", `<div class="actions">${["status", "start", "stop", "restart"].map(op => button(op[0].toUpperCase() + op.slice(1), "service-operation", id, `data-operation="${op}"`)).join("")}</div>`) : "")
    + section("Address", r.enabled ? '<p class="notice">Disable this route to edit its address or upstream.</p>' : routeForm(r))
    + section("Who sees this service", accessPanel(r)));
}

export async function folderPicker(path = "") {
  const result = await api(`/api/folders?path=${encodeURIComponent(path)}`);
  const parent = result.parent !== undefined && result.parent !== null;
  // Inside a folder, list each subfolder by its own name; the full path is above.
  const label = p => result.path ? p.split(/[\\/]/).filter(Boolean).pop() || p : p;
  modal("Choose a folder on this server", `<p class="mono path-display">${icon("folder")}${esc(result.path || "Available roots")}</p><div class="actions">${result.path ? button("Use this folder", "folder-select", "", `data-path="${esc(result.path)}"`, "primary", "check") : ""}${parent ? button("Parent folder", "folder-open", "", `data-path="${esc(result.parent)}"`, "subtle", "up") : ""}</div><div class="folder-list">${result.folders.map(p => button(label(p), "folder-open", "", `data-path="${esc(p)}"`, "subtle", "folder")).join("") || '<p class="help">No folders inside.</p>'}</div>`);
}
