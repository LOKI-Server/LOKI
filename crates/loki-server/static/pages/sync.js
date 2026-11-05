// Save sync: folders kept the same across a person's devices, with version history.
// Devices use /sync/v1; account settings manage their shared sign-in.
import { ago, api, enc, esc, fmt, modal, store } from "../lib/core.js";
import { icon } from "../lib/icons.js";
import { button, empty, field, heading, linkButton, table, tag } from "../lib/ui.js";

const kinds = { generic: "General files", "retroarch-saves": "RetroArch saves", "retroarch-states": "RetroArch save states", "emulator-saves": "Other emulator saves", "pc-game-saves": "PC game saves", documents: "Documents" };
const kindLabel = kind => kinds[kind] || kind;
const disabled = () => heading("Save sync", "Your saves and files, the same on every device.", "", "sync")
  + empty("Sync is not enabled for your account.", "Ask the server owner to give you access to Save sync.", "", "ᛇ");

export const folderUrl = (id, path = "", page = 0) => {
  const query = new URLSearchParams();
  if (path) query.set("path", path);
  if (page) query.set("page", page);
  return `/sync/${enc(id)}${query.size ? `?${query}` : ""}`;
};
const fileUrl = (id, path) => `/api/sync/folders/${enc(id)}/file?path=${enc(path)}`;
const historyFileUrl = (id, history) => `/api/sync/folders/${enc(id)}/file?history=${enc(history)}`;

export function syncPage() {
  const sync = store.state.sync;
  if (!sync) return disabled();
  const conflicts = sync.folders.reduce((n, f) => n + f.conflicts, 0);
  const folders = table(["Folder", "Kind", "Files", "Size", ""], sync.folders.map(f => ({
    key: f.id,
    cells: [
      `<a class="file-name" href="${esc(folderUrl(f.id))}"><span class="file-icon folder" aria-hidden="true">${icon("folder")}</span>${esc(f.name)}</a>${f.conflicts ? tag(`${f.conflicts} conflict${f.conflicts === 1 ? "" : "s"}`, "warn") : ""}`,
      `<span class="muted">${esc(kindLabel(f.kind))}</span>`,
      String(f.files),
      fmt(f.bytes),
      button("Manage", "sync-folder-manage", f.id, "", "ghost", "settings"),
    ],
  })));
  return heading("Save sync", "Your saves and files, the same on every device.", linkButton("Devices & sign-in", "/account", "subtle", "account"), "sync")
    + (conflicts ? `<div class="pending-banner">${icon("alert")}<span>${conflicts} conflicting version${conflicts === 1 ? "" : "s"} need${conflicts === 1 ? "s" : ""} a decision. Open the folder to choose.</span></div>` : "")
    + `<div class="split"><div class="stack"><section class="card"><div class="card-header"><h2>Folders</h2>${button("New folder", "sync-folder-new", "", "", "subtle", "plus")}</div>${sync.folders.length ? folders : '<p class="help">Create a folder, then map it to a local directory on each device, such as RetroArch’s saves folder.</p>'}</section></div>`
    + `<div class="stack"><section class="card"><h2>Storage</h2><progress class="quota" max="${sync.quota}" value="${Math.min(sync.usage, sync.quota)}" aria-label="Storage used"></progress><p><strong>${fmt(sync.usage)}</strong> of ${fmt(sync.quota)} used, including version history.</p><small>Files up to ${fmt(sync.max_file_bytes)} each.</small></section>`
    + `<section class="card"><h2>Device sign-in</h2><p>Pair devices and manage their access from your account.</p>${linkButton("Account & devices", "/account", "subtle", "account")}</section></div></div>`;
}

export async function folderPage(id, { path = "", page = 0 } = {}) {
  if (!store.state.sync) return disabled();
  const listing = await api(`/api/sync/folders/${enc(id)}?path=${enc(path)}&page=${page}`);
  const folder = listing.folder, summary = store.state.sync.folders.find(f => f.id === id);
  let prefix = "";
  const crumbs = path.split("/").filter(Boolean).map(part => {
    prefix += (prefix ? "/" : "") + part;
    return `${icon("chevron", "crumb-sep")}<a href="${esc(folderUrl(id, prefix))}">${esc(part)}</a>`;
  }).join("");
  const rows = listing.entries.map(e => {
    const full = path ? `${path}/${e.name}` : e.name;
    const name = e.directory
      ? `<a class="file-name" href="${esc(folderUrl(id, full))}"><span class="file-icon folder" aria-hidden="true">${icon("folder")}</span>${esc(e.name)}</a>`
      : `<span class="file-name"><span class="file-icon" aria-hidden="true">${icon("file")}</span>${esc(e.name)}</span>`;
    return {
      key: full,
      cells: [
        name,
        fmt(e.size),
        `<span class="file-meta">${e.modified ? ago(e.modified) : "—"}</span>`,
        e.directory ? "" : `<span class="muted">v${e.version} · ${esc(e.device)}</span>`,
        e.directory ? "" : `<div class="actions">${button("History", "sync-history", id, `data-path="${esc(full)}"`, "ghost", "archive")}<a class="icon-link" href="${esc(fileUrl(id, full))}" download aria-label="Download ${esc(e.name)}">${icon("download")}</a></div>`,
      ],
    };
  });
  const pager = [
    page > 0 ? linkButton("Previous", folderUrl(id, path, page - 1), "subtle", "left") : "",
    (page + 1) * 100 < listing.total ? linkButton("Next", folderUrl(id, path, page + 1), "subtle", "chevron") : "",
  ].join("");
  const conflicts = summary?.conflicts || 0;
  const actions = (conflicts ? button(`Resolve ${conflicts} conflict${conflicts === 1 ? "" : "s"}`, "sync-conflicts", id, "", "primary", "alert") : "") + button("Manage", "sync-folder-manage", id, "", "subtle", "settings");
  return heading(folder.name, `${kindLabel(folder.kind)} · keeps ${folder.keep_versions} earlier version${folder.keep_versions === 1 ? "" : "s"} of each file.`, actions, "sync")
    + `<div class="toolbar"><nav class="breadcrumbs" aria-label="Folder"><a href="/sync">${icon("left")}Save sync</a>${icon("chevron", "crumb-sep")}<a href="${esc(folderUrl(id))}">${esc(folder.name)}</a>${crumbs}</nav><div class="toolbar-end"><span class="muted">${listing.total} item${listing.total === 1 ? "" : "s"}</span></div></div>`
    + (listing.entries.length ? table(["Name", "Size", "Modified", "Version", ""], rows) : '<div class="empty small"><span class="empty-rune" aria-hidden="true">ᛇ</span><p>Nothing synced here yet.</p></div>')
    + (pager ? `<div class="actions pager">${pager}</div>` : "");
}

const reasons = { replaced: ["Replaced", "dim"], deleted: ["Deleted", "dim"], conflict: ["Conflict", "warn"] };
function historyRows(id, entries, showPath) {
  return entries.map(h => {
    const [label, kind] = reasons[h.reason] || [h.reason, "dim"];
    const open = !h.resolved;
    const controls = open
      ? button("Use this version", "sync-conflict-resolve", id, `data-history="${h.id}" data-keep="this"`, "primary", "check") + button("Keep current", "sync-conflict-resolve", id, `data-history="${h.id}" data-keep="current"`, "ghost", "close")
      : button("Restore", "sync-restore", id, `data-history="${h.id}"`, "ghost", "refresh");
    return {
      key: String(h.id),
      cells: [
        ...(showPath ? [`<span class="mono">${esc(h.path)}</span>`] : []),
        `v${h.version} ${tag(open ? "Needs decision" : label, open ? "warn" : kind)}`,
        `${esc(h.device_name)}<br><small>${ago(h.created)}</small>`,
        fmt(h.size),
        `<div class="actions">${h.hash ? `<a class="icon-link" href="${esc(historyFileUrl(id, h.id))}" download aria-label="Download version ${h.version}">${icon("download")}</a>` : ""}${h.hash ? controls : ""}</div>`,
      ],
    };
  });
}
export async function historyDialog(id, path) {
  const entries = await api(`/api/sync/folders/${enc(id)}/history?path=${enc(path)}`);
  modal(path.split("/").pop(), `<p>Earlier versions kept on the server. Restoring makes a version current again on every device; the version it replaces is kept here too.</p>`
    + table(["Version", "From", "Size", ""], historyRows(id, entries, false), "No earlier versions yet."));
}
export async function conflictsDialog(id) {
  const entries = await api(`/api/sync/folders/${enc(id)}/history?conflicts=true`);
  modal("Resolve conflicts", `<p>Two devices changed these files from the same starting point. Nothing was lost: choose which version every device should use.</p>`
    + table(["File", "Version", "From", "Size", ""], historyRows(id, entries, true), "No conflicts remain."));
}

export function folderForm(f = {}) {
  const options = Object.entries(kinds).map(([value, label]) => `<option value="${value}" ${f.kind === value ? "selected" : ""}>${esc(label)}</option>`).join("");
  return `<form data-form="sync-folder" data-id="${esc(f.id || "")}">${field("name", "Folder name", f.name || "", "text", "required maxlength=100 placeholder=\"GBA saves\"")}${f.id ? field("keep_versions", "Earlier versions to keep per file", f.keep_versions, "number", "required min=1 max=1000") : `<label for="sync-kind">What it holds</label><select id="sync-kind" name="kind">${options}</select><p class="help">A hint for sync clients. Each device chooses its own local directory for this folder.</p>`}<div class="actions"><button>${f.id ? "Save folder" : "Create folder"}</button></div></form>`;
}
export function folderManage(id) {
  const f = store.state.sync.folders.find(f => f.id === id);
  modal(f.name, folderForm(f) + `<section class="dialog-section"><h3>Delete this folder</h3><p>Removes the folder, its files, and their history from the server. Files already on your devices stay there, but stop syncing.</p><form data-form="sync-folder-delete" data-id="${esc(id)}">${field("confirm", `Type “${f.name}” to confirm`, "", "text", "required autocomplete=off")}<div class="actions"><button class="danger">${icon("trash")}Delete folder</button></div></form></section>`);
}
export function quotaSection(userId) {
  const sync = store.state.sync;
  if (!sync) return "";
  const custom = sync.quotas?.[userId];
  return `<section class="dialog-section"><h3>Save sync storage</h3><form data-form="sync-quota" data-id="${esc(userId)}">${field("mb", "Quota in MiB (blank uses the server default)", custom === undefined ? "" : Math.round(custom / 1048576), "number", "min=0")}<div class="actions"><button>Save quota</button></div></form></section>`;
}
