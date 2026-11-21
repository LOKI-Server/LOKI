// Owner dialogs for managing applications, shares, routes, built-in features, and who can reach them.
import { api, modal, resource, store } from "../lib/core.js";
import { html } from "../lib/html.js";
import { icon } from "../lib/icons.js";
import { Badge, Button, ButtonGroup, Checkbox, DialogSection, Field, FieldRow, Form, FormFooter, Help, IconButton, Notice, Select, Submit } from "../lib/ui.js";
import { resourceStatus } from "./services.js";

export const subjects = () => [
  ...store.state.groups.map(g => ({ key: `group:${g.id}`, label: `Group: ${g.name}` })),
  ...store.state.users.map(u => ({ key: `user:${u.id}`, label: `Person: ${u.username}` })),
];
export const roles = r => [...new Set(["access", ...(r.kind === "app" ? ["admin", ...r.manifest.roles] : [])])];

/** Who can reach a resource: an audience, and permissions for people and groups. */
export function accessPanel(r) {
  const grants = store.state.grants.filter(g => g.resource === r.id);
  const creation = ["vcs-create", "vcs-group-create"].includes(r.kind);
  const audiences = { restricted: "Selected people and groups", ...(creation ? {} : { members: "All approved members" }), ...(["sync", "vcs-create", "vcs-group-create"].includes(r.kind) ? {} : { public: "Public" }) };
  const note = r.kind === "app" ? "Public makes the service discoverable, but the app may still ask for a LOKI sign-in."
    : r.kind === "route" && !r.gated ? "Without the sign-in gate, these permissions control who sees the service in LOKI. The service itself controls sign-in." : "";
  return Form({ name: "access", id: r.id, body: html`${Select({ name: "visibility", label: "Audience", options: Object.entries(audiences), value: r.visibility, help: note })}<div class="field"><span class="field-label">Permissions</span><div id="grants" class="grant-list">${grants.map(g => grantRow(subjects(), roles(r), `${g.subject_kind}:${g.subject}`, g.role))}</div>${Button({ label: "Add permission", action: "grant-add", id: r.id, icon: "plus", size: "sm", kind: "ghost" })}</div>${FormFooter(Submit({ label: "Save access" }))}` });
}

export function grantRow(subjects, roles, selected = "", role = "access") {
  return html`<div class="grant-row"><select class="grant-subject" aria-label="Person or group">${subjects.map(s => html`<option value="${s.key}"${s.key === selected ? html` selected` : ""}>${s.label}</option>`)}</select><select class="grant-role" aria-label="Role">${roles.map(r => html`<option${r === role ? html` selected` : ""}>${r}</option>`)}</select>${IconButton({ icon: "trash", label: "Remove permission", action: "grant-remove" })}</div>`;
}

export function syncManage(id) {
  const r = resource(id), s = store.state.settings;
  modal("Save sync", [html`<p class="lead">Each person only ever sees their own folders. Storage counts every file's version history.</p>`,
    DialogSection("Storage", Form({ name: "settings", body: html`${FieldRow(Field({ name: "sync_quota_mb", label: "Storage per person (MiB)", value: s.sync_quota_mb, type: "number", required: true, min: 1 }), Field({ name: "sync_max_file_mb", label: "Largest file (MiB)", value: s.sync_max_file_mb, type: "number", required: true, min: 1, max: 1048576 }))}<p class="help">Give someone more or less room from their page in <a href="/accounts">People &amp; groups</a>.</p>${FormFooter(Submit({ label: "Save storage" }))}` })),
    DialogSection("Who can use sync", accessPanel(r))]);
}

export const vcsLimits = { vcs_repo_mib: "Storage per repository (MiB)", vcs_total_mib: "Total repository storage (MiB)" };

export function vcsManage() {
  const s = store.state.settings;
  modal("Repositories", html`<p class="lead">Repositories are hosted by Skald. Each one keeps its own access list; these settings apply to all of them.</p>${DialogSection("Storage", Form({ name: "settings", body: html`${FieldRow(...Object.entries(vcsLimits).map(([key, label]) => Field({ name: key, label, value: s[key], type: "number", required: true, min: 1, max: 1048576 })))}${FormFooter(Submit({ label: "Save storage" }))}` }))}${DialogSection("Who can create repositories", accessPanel(resource("vcs-create")))}${resource("vcs-group-create") ? DialogSection("Who can create repositories in their groups", html`<p class="help">People chosen here can create repositories owned by groups they belong to. You can always create them in any group.</p>${accessPanel(resource("vcs-group-create"))}`) : ""}`);
}

// Apps that keep files can be given more room, or less, than they ask for.
function storageSettings(r) {
  if (!r.manifest.capabilities?.includes("storage")) return "";
  const asked = r.manifest.limits?.storage_mib ?? 64;
  return Field({ name: "storage_mib", label: "Storage quota (MiB)", value: r.storage_mib ?? "", type: "number", min: 1, max: 1048576, placeholder: asked, help: `Leave blank for the ${asked} MiB the app asks for.` });
}

// Apps with live connections, such as game servers, can also listen on the LAN.
function realtimeSettings(r) {
  if (!r.manifest.capabilities?.includes("realtime")) return "";
  const path = r.manifest.limits?.realtime?.path || "/multiplayer";
  return html`${Notice({ tone: "info", body: html`Anyone who can reach this app can connect to its live endpoint (<span class="mono">${path}</span>) without a LOKI account. The app decides who may join.` })}${Field({ name: "lan_bind", label: "LAN address (optional)", value: r.lan_bind || "", placeholder: "0.0.0.0:4767", help: "Also accept connections on this address, so devices on your network can connect by IP. Only the live endpoint and health check answer there. Allow the port in your firewall." })}`;
}

function appField(r, f) {
  const value = r.config[f.key] ?? f.default;
  if (f.kind === "bool") return Checkbox({ name: f.key, label: f.label, checked: value });
  return Field({ name: f.key, label: f.label, value: value ?? "", type: f.kind === "secret" ? "password" : f.kind === "number" ? "number" : "text", required: f.required });
}

export function appManage(id) {
  const r = resource(id);
  const operation = (label, op, kind = "secondary", iconName = "") => Button({ label, action: "app-operation", id, data: { operation: op }, kind, icon: iconName, size: "sm" });
  modal(r.name, [html`<p class="lead">${r.manifest.description}</p><div class="badge-row">${Badge(`v${r.manifest.version}`, "neutral")}${Badge(r.manifest.runtime === "wasi" ? "WASI component" : "Native service", "neutral")}${resourceStatus(r)}</div>${r.error ? Notice({ tone: "danger", body: r.error }) : ""}${ButtonGroup(operation(r.enabled ? "Disable" : "Enable", r.enabled ? "disable" : "enable", "primary", "power"), r.enabled ? operation("Restart", "restart", "secondary", "refresh") : "", r.manifest.panel && r.ready ? Button({ label: "Open panel", action: "app-panel", id, icon: "apps", size: "sm" }) : "", Button({ label: "Logs", action: "logs", id, icon: "logs", size: "sm" }), r.previous ? operation("Roll back version & data", "rollback", "secondary", "left") : "")}`,
    DialogSection("Settings", Form({ name: "app-config", id, body: html`${Field({ name: "hostname", label: "Public hostname (optional)", value: r.hostname, placeholder: "bigshot.example.com", help: "Leave blank for loopback access. Your domain needs DNS and a configured Caddy." })}${realtimeSettings(r)}${storageSettings(r)}${r.manifest.fields.map(f => appField(r, f))}${FormFooter(Submit({ label: "Save settings" }))}` })),
    r.manifest.capabilities?.includes("personal-storage") ? DialogSection("Personal storage", Form({ name: "personal-enable", id, body: html`${Checkbox({ name: "enabled", label: "Allow this app to save files for its signed-in users", checked: r.personal_storage })}${FormFooter(Submit({ label: "Save permission" }))}` })) : "",
    DialogSection("Who can use this app", accessPanel(r)),
    DialogSection("Uninstall", html`<div class="danger-row"><p>Removes the app. Its data and shared files are kept.</p>${operation("Uninstall", "uninstall", "danger", "trash")}</div>`)]);
}

export function shareForm(r = {}) {
  const excluded = Array.isArray(r.excluded_extensions) ? r.excluded_extensions.map(e => "." + e).join(", ") : (r.excluded_extensions || "");
  const type = r.id
    ? html`<p class="field-static"><span class="field-label">Type</span>${Badge(r.share_type === "apps" ? "App repository" : "File server", "neutral")}</p>`
    : Select({ name: "share_type", label: "Kind of share", options: [["files", "File server"], ["apps", "App repository"]], value: r.share_type, help: "For an app repository, put each app in its own folder. LOKI Desktop offers its builds for download and updates." });
  return Form({ name: "share", id: r.id || "", body: html`${Field({ name: "name", label: "Share name", value: r.name || "", required: true })}${type}<div class="input-with-button">${Field({ name: "root", label: "Folder on this server", value: r.root || "", required: true })}${Button({ label: "Browse", action: "folder-picker", id: r.id || "", icon: "folder" })}</div>${Field({ name: "excluded_extensions", label: "Ignore file extensions (optional)", value: excluded, maxlength: 4096, placeholder: ".tmp, .bak, .psd, .tar.gz", help: "Separate extensions with commas or spaces. Matching ignores case and applies in every folder. These files are hidden from listings and can't be previewed or downloaded through this share." })}${r.id ? "" : Help("New shares are restricted to you until you grant access.")}${FormFooter(Submit({ label: r.id ? "Save share" : "Create share" }))}` });
}
export function shareManage(id, draft = {}) {
  const r = resource(id);
  modal(r.name, html`${shareForm({ ...r, ...draft, id })}${DialogSection("Who can browse this share", accessPanel(r))}${DialogSection("Remove share", html`<div class="danger-row"><p>Stops sharing the folder through LOKI. Files on disk are kept.</p>${Button({ label: "Remove share", action: "share-delete", id, kind: "danger", icon: "trash", size: "sm" })}</div>`)}`);
}

export function routeForm(r = {}) {
  const adapters = [["none", "No OS service"], ["windows", "Windows service"], ["systemd", "Linux systemd"], ["brew", "macOS Homebrew"]];
  return Form({ name: "route", id: r.id || "", body: html`${Field({ name: "name", label: "Service name", value: r.name || "" })}${Field({ name: "hostname", label: "Public hostname", value: r.hostname || "", placeholder: "jellyfin.example.com", required: true })}${Field({ name: "upstream", label: "Upstream origin", value: r.upstream || "http://127.0.0.1:8096", type: "url", required: true })}${Checkbox({ name: "gated", label: "Require LOKI browser sign-in before forwarding", checked: r.gated, help: "Leave this off for Jellyfin and services used by native apps. They keep their own sign-in." })}${FieldRow(Select({ name: "service_kind", label: "OS service adapter (optional)", options: adapters, value: r.service_kind }), Field({ name: "service", label: "OS service name", value: r.service || "" }))}${FormFooter(Submit({ label: "Save route" }))}` });
}
export function routeManage(id) {
  const r = resource(id);
  const operation = (label, op, kind, iconName) => Button({ label, action: "route-operation", id, data: { operation: op }, kind, icon: iconName, size: "sm" });
  modal(r.name, [html`<div class="badge-row">${resourceStatus(r)}${r.gated ? Badge("Sign-in gate") : ""}</div>${ButtonGroup(operation(r.enabled ? "Disable route" : "Enable route", r.enabled ? "disable" : "enable", "primary", "power"))}`,
    r.service ? DialogSection("OS service", ButtonGroup(...["status", "start", "stop", "restart"].map(op => Button({ label: op[0].toUpperCase() + op.slice(1), action: "service-operation", id, data: { operation: op }, size: "sm" })))) : "",
    DialogSection("Address", r.enabled ? Notice({ tone: "info", body: "Disable this route to edit its address or upstream." }) : routeForm(r)),
    DialogSection("Who sees this service", accessPanel(r)),
    DialogSection("Delete route", html`<div class="danger-row"><p>LOKI stops forwarding to the service. The service itself keeps running.</p>${operation("Delete route", "delete", "danger", "trash")}</div>`)]);
}

export async function folderPicker(path = "") {
  const result = await api(`/api/folders?path=${encodeURIComponent(path)}`);
  const parent = result.parent !== undefined && result.parent !== null;
  // Inside a folder, list each subfolder by its own name; the full path is above.
  const label = p => result.path ? p.split(/[\\/]/).filter(Boolean).pop() || p : p;
  modal("Choose a folder on this server", html`<p class="path-display mono">${icon("folder")}${result.path || "Available roots"}</p>${ButtonGroup(result.path ? Button({ label: "Use this folder", action: "folder-select", data: { path: result.path }, kind: "primary", icon: "check", size: "sm" }) : "", parent ? Button({ label: "Parent folder", action: "folder-open", data: { path: result.parent }, icon: "up", size: "sm" }) : "")}<div class="folder-list">${result.folders.length ? result.folders.map(p => html`<button type="button" class="folder-item" data-action="folder-open" data-path="${p}">${icon("folder")}<span>${label(p)}</span></button>`) : Help("No folders inside.")}</div>`);
}
