// Owner dialogs for managing applications, shares, routes, built-in features, and who can reach them.
import { api, modal, resource, store } from "../lib/core.js";
import { html } from "../lib/html.js";
import { icon } from "../lib/icons.js";
import { Badge, Button, ButtonGroup, Checkbox, DialogSection, Field, FieldRow, Form, FormFooter, Help, IconButton, List, Notice, PanelTabs, Row, Select, SizeField, Submit, formatSize } from "../lib/ui.js";
import { avatar } from "./people.js";
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
    DialogSection("Storage", Form({ name: "settings", body: html`${FieldRow(SizeField({ name: "sync_quota_mb", label: "Storage per person", mib: s.sync_quota_mb, required: true, max: 100000000 }), SizeField({ name: "sync_max_file_mb", label: "Largest file", mib: s.sync_max_file_mb, required: true }))}<p class="help">Give someone more or less room from their page in <a href="/accounts">People &amp; groups</a>.</p>${FormFooter(Submit({ label: "Save storage" }))}` })),
    DialogSection("Who can use sync", accessPanel(r))]);
}

export const vcsLimits = { vcs_repo_mib: "Each repository", vcs_total_mib: "All repositories together" };

/** A person's own repository limits, described in a few words. */
const quotaSummary = q => [q.repo_mib ? `${formatSize(q.repo_mib)} per repository` : "", q.total_mib ? `${formatSize(q.total_mib)} in total` : ""].filter(Boolean).join(" · ");

/** Storage defaults and per-person limits, and who can create repositories. */
export function vcsManage(tab = "storage") {
  const s = store.state.settings, quotas = store.state.vcs_quotas || {};
  const people = store.state.users.filter(u => quotas[u.id]);
  const defaults = Form({ name: "settings", body: html`${FieldRow(
    SizeField({ name: "vcs_repo_mib", label: vcsLimits.vcs_repo_mib, mib: s.vcs_repo_mib, required: true }),
    SizeField({ name: "vcs_total_mib", label: vcsLimits.vcs_total_mib, mib: s.vcs_total_mib, required: true, help: "The most the whole server holds." }))}${FormFooter(Submit({ label: "Save defaults" }))}` });
  const overrides = html`${Help("Give someone more or less room for repositories they own. Group repositories use the defaults.")}${people.length
    ? List(people.map(u => Row({ key: u.id, lead: avatar(u, "sm"), title: u.display_name || u.username, meta: quotaSummary(quotas[u.id]),
        actions: ButtonGroup(Button({ label: "Edit", action: "vcs-quota-edit", id: u.id, size: "sm" }), IconButton({ icon: "trash", label: `Remove ${u.username}'s limits`, action: "vcs-quota-clear", id: u.id, size: "sm" })) })), { className: "list-boxed" })
    : ""}${Button({ label: "Set limits for someone", action: "vcs-quota-edit", icon: "plus", size: "sm", kind: people.length ? "ghost" : "secondary" })}`;
  const creation = html`${DialogSection("Personal repositories", html`${Help("Who can create repositories of their own.")}${accessPanel(resource("vcs-create"))}`)}${resource("vcs-group-create") ? DialogSection("Group repositories", html`${Help("Who can create repositories owned by a group they're a member of. You can always create them in any group.")}${accessPanel(resource("vcs-group-create"))}`) : ""}`;
  modal("Repositories", html`<p class="lead">Repositories are hosted by Skald. Each one keeps its own access list.</p>${PanelTabs([
    { id: "storage", label: "Storage", icon: "database", body: html`${DialogSection("Defaults", defaults)}${DialogSection("Per-person limits", overrides)}` },
    { id: "creation", label: "Who can create", icon: "people", body: creation },
  ], { active: tab, label: "Repository settings" })}`);
}

/**
 * One person's repository limits. Blank boxes use the defaults. Without `userId`
 * it starts with a choice of person. `back` names the dialog to return to.
 */
export function vcsQuotaForm(userId, back = "vcs") {
  const s = store.state.settings, q = store.state.vcs_quotas?.[userId] || {};
  const choose = userId ? "" : Select({ name: "user", label: "Person", options: store.state.users.filter(u => u.status === "approved" && !store.state.vcs_quotas?.[u.id]).map(u => [u.id, u.display_name ? `${u.display_name} (@${u.username})` : `@${u.username}`]) });
  return Form({ name: "vcs-quota", id: userId || "", data: { back }, body: html`${choose}${FieldRow(
    SizeField({ name: "repo_mib", label: "Each repository", mib: q.repo_mib, placeholder: `Default, ${formatSize(s.vcs_repo_mib)}` }),
    SizeField({ name: "total_mib", label: "All their repositories", mib: q.total_mib, max: 100 * 1048576, placeholder: "No limit of their own" }))}${Help("Leave a box blank to use the default. The server-wide total still applies.")}${back === "vcs"
    ? FormFooter([Button({ label: "Back", action: "vcs-manage", icon: "left", kind: "ghost" }), Submit({ label: "Save limits" })], { align: "between" })
    : FormFooter(Submit({ label: "Save limits" }))}` });
}

/** The repository storage part of a person's page in People & groups. */
export const vcsQuotaSection = userId => DialogSection("Repository storage", vcsQuotaForm(userId, "person"));

export function vcsQuotaDialog(userId) {
  const u = store.state.users.find(u => u.id === userId);
  modal(u ? `Repository storage for ${u.display_name || u.username}` : "Set repository limits", vcsQuotaForm(userId));
}

// Apps that keep files can be given more room, or less, than they ask for.
function storageSettings(r) {
  if (!r.manifest.capabilities?.includes("storage")) return "";
  const asked = r.manifest.limits?.storage_mib ?? 64;
  return SizeField({ name: "storage_mib", label: "Storage quota", mib: r.storage_mib, placeholder: `Default, ${formatSize(asked)}`, help: `Leave blank for the ${formatSize(asked)} the app asks for.` });
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
