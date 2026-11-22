// Save sync: folders kept the same across a person's devices, with version history.
// Devices use /sync/v1; account settings manage their shared sign-in.
import { ago, api, enc, fmt, modal, store } from "../lib/core.js";
import { html, plural } from "../lib/html.js";
import { icon } from "../lib/icons.js";
import { Badge, Breadcrumbs, Button, Card, DialogSection, EmptyState, Field, Form, FormFooter, IconButton, IconLink, LinkButton, List, Meter, Notice, PageHeader, Pager, Row, Select, SizeField, Split, Submit, Table, Toolbar, formatSize } from "../lib/ui.js";

const kinds = { generic: "General files", "retroarch-saves": "RetroArch saves", "retroarch-states": "RetroArch save states", "emulator-saves": "Other emulator saves", "pc-game-saves": "PC game saves", documents: "Documents" };
const kindLabel = kind => kinds[kind] || kind;
const header = actions => PageHeader({ title: "Save sync", description: "Your saves and files, the same on every device.", actions, realm: "sync" });
const disabled = () => html`${header()}${EmptyState({ title: "Sync is not enabled for your account.", text: "Ask the server owner to give you access to Save sync.", rune: "ᛇ" })}`;

export const folderUrl = (id, path = "", page = 0) => {
  const query = new URLSearchParams();
  if (path) query.set("path", path);
  if (page) query.set("page", page);
  return `/sync/${enc(id)}${query.size ? `?${query}` : ""}`;
};
const fileUrl = (id, path) => `/api/sync/folders/${enc(id)}/file?path=${enc(path)}`;
const historyFileUrl = (id, history) => `/api/sync/folders/${enc(id)}/file?history=${enc(history)}`;
const conflictCount = n => Badge(`${n} conflict${n === 1 ? "" : "s"}`, "warn");

export function syncPage() {
  const sync = store.state.sync;
  if (!sync) return disabled();
  const conflicts = sync.folders.reduce((n, f) => n + f.conflicts, 0);
  const owned = store.session?.user?.owner && store.state.resources?.find(r => r.kind === "sync");
  const folders = sync.folders.map(f => Row({
    key: f.id, icon: "folder", tone: "gold", title: f.name, href: folderUrl(f.id),
    badges: f.conflicts ? conflictCount(f.conflicts) : "",
    meta: html`<span>${kindLabel(f.kind)}</span><span>${plural(f.files, "file")}</span><span>${fmt(f.bytes)}</span>`,
    actions: IconButton({ icon: "settings", label: `Manage ${f.name}`, action: "sync-folder-manage", id: f.id, size: "sm" }),
  }));
  const main = Card({
    title: "Folders", description: "Each device chooses its own local folder for each of these.",
    actions: Button({ label: "New folder", action: "sync-folder-new", icon: "plus", size: "sm" }),
    flush: true,
    body: folders.length ? List(folders) : EmptyState({ compact: true, rune: "ᛇ", text: "Create a folder, then map it to a local directory on each device, such as RetroArch’s saves folder." }),
  });
  const side = [
    Card({ title: "Storage", body: html`${Meter({ value: sync.usage, max: sync.quota, label: "Storage used" })}<p class="meter-caption"><strong>${fmt(sync.usage)}</strong> of ${fmt(sync.quota)} used, including version history.</p><p class="help">Files up to ${fmt(sync.max_file_bytes)} each.</p>` }),
    Card({ title: "Devices", body: html`<p>Pair a device with a code, then choose a local folder for each sync folder on it. Rename or sign out devices from your account.</p>${LinkButton({ label: "Your devices", href: "/account?tab=devices", icon: "server", size: "sm" })}` }),
  ];
  return html`${header(owned ? Button({ label: "Manage", action: "sync-manage", id: owned.id, icon: "settings" }) : "")}${conflicts ? Notice({ tone: "warn", body: `${conflicts} conflicting version${conflicts === 1 ? "" : "s"} need${conflicts === 1 ? "s" : ""} a decision. Open the folder to choose.` }) : ""}${Split(main, side)}`;
}

export async function folderPage(id, { path = "", page = 0 } = {}) {
  if (!store.state.sync) return disabled();
  const listing = await api(`/api/sync/folders/${enc(id)}?path=${enc(path)}&page=${page}`);
  const folder = listing.folder, summary = store.state.sync.folders.find(f => f.id === id);
  let prefix = "";
  const crumbs = [{ label: "Save sync", href: "/sync", icon: "left" }, { label: folder.name, href: path ? folderUrl(id) : "" }, ...path.split("/").filter(Boolean).map((part, i, all) => {
    prefix += (prefix ? "/" : "") + part;
    return { label: part, href: i === all.length - 1 ? "" : folderUrl(id, prefix) };
  })];
  const rows = listing.entries.map(e => {
    const full = path ? `${path}/${e.name}` : e.name;
    const name = e.directory
      ? html`<a class="file-name" href="${folderUrl(id, full)}"><span class="file-icon folder" aria-hidden="true">${icon("folder")}</span>${e.name}</a>`
      : html`<span class="file-name"><span class="file-icon" aria-hidden="true">${icon("file")}</span>${e.name}</span>`;
    return {
      key: full,
      cells: [
        name,
        e.directory ? html`<span class="muted">—</span>` : fmt(e.size),
        html`<span class="muted nowrap">${e.modified ? ago(e.modified) : "—"}</span>`,
        e.directory ? "" : html`<span class="muted">v${e.version} · ${e.device}</span>`,
        e.directory ? "" : html`<div class="cell-actions">${Button({ label: "History", action: "sync-history", id, data: { path: full }, icon: "history", size: "sm", kind: "ghost" })}${IconLink({ icon: "download", label: `Download ${e.name}`, href: fileUrl(id, full), download: true, size: "sm" })}</div>`,
      ],
    };
  });
  const conflicts = summary?.conflicts || 0;
  const actions = [conflicts ? Button({ label: `Resolve ${plural(conflicts, "conflict")}`, action: "sync-conflicts", id, kind: "primary", icon: "alert" }) : "", Button({ label: "Manage", action: "sync-folder-manage", id, icon: "settings" })];
  return html`${PageHeader({ title: folder.name, description: `${kindLabel(folder.kind)} · keeps ${plural(folder.keep_versions, "earlier version")} of each file.`, actions, realm: "sync" })}${Toolbar({ start: Breadcrumbs(crumbs, "Folder"), end: html`<span class="muted">${plural(listing.total, "item")}</span>` })}${listing.entries.length ? Table({ columns: ["Name", "Size", "Modified", "Version", ""], rows }) : EmptyState({ compact: true, rune: "ᛇ", text: "Nothing synced here yet." })}${Pager({ previous: page > 0 ? folderUrl(id, path, page - 1) : "", next: (page + 1) * 100 < listing.total ? folderUrl(id, path, page + 1) : "" })}`;
}

const reasons = { replaced: ["Replaced", "neutral"], deleted: ["Deleted", "neutral"], conflict: ["Conflict", "warn"] };
function historyRows(id, entries, showPath) {
  return entries.map(h => {
    const [label, tone] = reasons[h.reason] || [h.reason, "neutral"];
    const open = !h.resolved;
    const controls = open
      ? [Button({ label: "Use this version", action: "sync-conflict-resolve", id, data: { history: h.id, keep: "this" }, kind: "primary", icon: "check", size: "sm" }), Button({ label: "Keep current", action: "sync-conflict-resolve", id, data: { history: h.id, keep: "current" }, kind: "ghost", size: "sm" })]
      : Button({ label: "Restore", action: "sync-restore", id, data: { history: h.id }, kind: "ghost", icon: "refresh", size: "sm" });
    return {
      key: String(h.id),
      cells: [
        ...(showPath ? [html`<span class="mono">${h.path}</span>`] : []),
        html`v${h.version} ${Badge(open ? "Needs decision" : label, open ? "warn" : tone)}`,
        html`${h.device_name}<small class="muted block">${ago(h.created)}</small>`,
        fmt(h.size),
        h.hash ? html`<div class="cell-actions">${IconLink({ icon: "download", label: `Download version ${h.version}`, href: historyFileUrl(id, h.id), download: true, size: "sm" })}${controls}</div>` : "",
      ],
    };
  });
}
export async function historyDialog(id, path) {
  const entries = await api(`/api/sync/folders/${enc(id)}/history?path=${enc(path)}`);
  modal(path.split("/").pop(), html`<p class="lead">Earlier versions kept on the server. Restoring makes a version current again on every device; the version it replaces is kept here too.</p>${Table({ columns: ["Version", "From", "Size", ""], rows: historyRows(id, entries, false), empty: "No earlier versions yet." })}`);
}
export async function conflictsDialog(id) {
  const entries = await api(`/api/sync/folders/${enc(id)}/history?conflicts=true`);
  modal("Resolve conflicts", html`<p class="lead">Two devices changed these files from the same starting point. Nothing was lost: choose which version every device should use.</p>${Table({ columns: ["File", "Version", "From", "Size", ""], rows: historyRows(id, entries, true), empty: "No conflicts remain." })}`);
}

export function folderForm(f = {}) {
  return Form({ name: "sync-folder", id: f.id || "", body: html`${Field({ name: "name", label: "Folder name", value: f.name || "", required: true, maxlength: 100, placeholder: "GBA saves" })}${f.id
    ? Field({ name: "keep_versions", label: "Earlier versions to keep per file", value: f.keep_versions, type: "number", required: true, min: 1, max: 1000 })
    : Select({ name: "kind", label: "What it holds", options: Object.entries(kinds), value: f.kind, help: "A hint for sync clients. Each device chooses its own local directory for this folder." })}${FormFooter(Submit({ label: f.id ? "Save folder" : "Create folder" }))}` });
}
export function folderManage(id) {
  const f = store.state.sync.folders.find(f => f.id === id);
  modal(f.name, html`${folderForm(f)}${DialogSection("Delete this folder", html`<p>Removes the folder, its files, and their history from the server. Files already on your devices stay there, but stop syncing.</p>${Form({ name: "sync-folder-delete", id, body: html`${Field({ name: "confirm", label: `Type “${f.name}” to confirm`, required: true, autocomplete: "off" })}${FormFooter(Submit({ label: "Delete folder", kind: "danger", icon: "trash" }))}` })}`)}`);
}
export function quotaSection(userId) {
  const sync = store.state.sync;
  if (!sync) return "";
  const custom = sync.quotas?.[userId];
  return DialogSection("Save sync storage", Form({ name: "sync-quota", id: userId, body: html`${SizeField({ name: "mb", label: "Storage", mib: custom === undefined ? null : Math.round(custom / 1048576), max: 100000000, placeholder: `Default, ${formatSize(store.state.settings?.sync_quota_mb ?? 0)}`, help: "Leave blank to use the server default." })}${FormFooter(Submit({ label: "Save quota" }))}` }));
}
