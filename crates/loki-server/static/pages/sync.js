// Save sync: folders kept the same across a person's devices, with version history.
// Devices use /sync/v1; account settings manage their shared sign-in.
import { ago, api, enc, fmt, modal, store } from "../lib/core.js";
import { html, plural } from "../lib/html.js";
import { icon } from "../lib/icons.js";
import { Badge, Breadcrumbs, Button, Card, Checkbox, DialogSection, EmptyState, Field, Form, FormFooter, IconButton, IconLink, KeyValue, LinkButton, List, Meter, Notice, PageHeader, Pager, Row, Select, Split, Submit, Table, Toolbar } from "../lib/ui.js";

const kinds = { generic: "General files", "retroarch-saves": "RetroArch saves", "retroarch-states": "RetroArch save states", "emulator-saves": "Other emulator saves", "pc-game-saves": "PC game saves", documents: "Documents" };
const kindLabel = kind => kinds[kind] || kind;
const header = actions => PageHeader({ title: "Save sync", description: "Your saves and files, the same on every device.", actions, realm: "sync" });
const disabled = () => html`${header()}${EmptyState({ title: "Sync is not enabled for your account.", text: "Ask the server owner to give you access to Save sync.", rune: "ᛇ" })}`;

export const folderUrl = (id, path = "", page = 0, deleted = false) => {
  const query = new URLSearchParams();
  if (path) query.set("path", path);
  if (page) query.set("page", page);
  if (deleted) query.set("deleted", "1");
  return `/sync/${enc(id)}${query.size ? `?${query}` : ""}`;
};
const fileUrl = (id, path) => `/api/sync/folders/${enc(id)}/file?path=${enc(path)}`;
const archiveUrl = (id, time) => `/api/sync/folders/${enc(id)}/archive${time ? `?time=${time}` : ""}`;
const when = (seconds, precise = false) => new Date(seconds * 1000).toLocaleString(undefined, { dateStyle: "medium", timeStyle: precise ? "medium" : "short" });
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

export async function folderPage(id, { path = "", page = 0, deleted = false } = {}) {
  if (!store.state.sync) return disabled();
  const listing = await api(`/api/sync/folders/${enc(id)}?path=${enc(path)}&page=${page}${deleted ? "&deleted=true" : ""}`);
  const folder = listing.folder, summary = store.state.sync.folders.find(f => f.id === id);
  let prefix = "";
  const crumbs = [{ label: "Save sync", href: "/sync", icon: "left" }, { label: folder.name, href: path ? folderUrl(id, "", 0, deleted) : "" }, ...path.split("/").filter(Boolean).map((part, i, all) => {
    prefix += (prefix ? "/" : "") + part;
    return { label: part, href: i === all.length - 1 ? "" : folderUrl(id, prefix, 0, deleted) };
  })];
  const rows = listing.entries.map(e => {
    const full = path ? `${path}/${e.name}` : e.name;
    const gone = e.deleted ? html` ${Badge("Deleted", "neutral")}` : "";
    const name = e.directory
      ? html`<a class="file-name" href="${folderUrl(id, full, 0, deleted)}"><span class="file-icon folder" aria-hidden="true">${icon("folder")}</span>${e.name}${gone}</a>`
      : html`<span class="${e.deleted ? "file-name muted" : "file-name"}"><span class="file-icon" aria-hidden="true">${icon("file")}</span>${e.name}${gone}</span>`;
    const history = Button({ label: e.deleted ? "Restore…" : "History", action: "sync-history", id, data: { path: full }, icon: "history", size: "sm", kind: "ghost" });
    return {
      key: full,
      cells: [
        name,
        e.directory ? html`<span class="muted">—</span>` : fmt(e.size),
        html`<span class="muted nowrap">${e.modified ? html`${e.deleted && !e.directory ? "Deleted " : ""}${ago(e.modified)}` : "—"}</span>`,
        e.directory ? "" : html`<span class="muted">${e.deleted ? "Deleted" : `v${e.version}`} · ${e.device}</span>`,
        e.directory ? "" : html`<div class="cell-actions">${history}${e.deleted ? "" : IconLink({ icon: "download", label: `Download ${e.name}`, href: fileUrl(id, full), download: true, size: "sm" })}</div>`,
      ],
    };
  });
  const conflicts = summary?.conflicts || 0;
  const actions = [
    conflicts ? Button({ label: `Resolve ${plural(conflicts, "conflict")}`, action: "sync-conflicts", id, kind: "primary", icon: "alert" }) : "",
    Button({ label: "Roll back…", action: "sync-rollback-open", id, icon: "history" }),
    LinkButton({ label: "Download ZIP", href: archiveUrl(id), download: true, icon: "download" }),
    Button({ label: "Manage", action: "sync-folder-manage", id, icon: "settings" }),
  ];
  const toggle = LinkButton({ label: deleted ? "Hide deleted files" : "Show deleted files", href: folderUrl(id, path, 0, !deleted), icon: "trash", size: "sm", kind: "ghost" });
  return html`${PageHeader({ title: folder.name, description: `${kindLabel(folder.kind)} · keeps ${plural(folder.keep_versions, "earlier version")} of each file.`, actions, realm: "sync" })}${Toolbar({ start: Breadcrumbs(crumbs, "Folder"), end: html`${toggle}<span class="muted">${plural(listing.total, "item")}</span>` })}${listing.entries.length ? Table({ columns: ["Name", "Size", "Modified", "Version", ""], rows }) : EmptyState({ compact: true, rune: "ᛇ", text: deleted ? "Nothing here, deleted or not." : "Nothing synced here yet." })}${Pager({ previous: page > 0 ? folderUrl(id, path, page - 1, deleted) : "", next: (page + 1) * 100 < listing.total ? folderUrl(id, path, page + 1, deleted) : "" })}`;
}

const changeText = g => [g.deleted && `${plural(g.deleted, "file")} deleted`, g.replaced && `${plural(g.replaced, "file")} replaced`, g.conflict && plural(g.conflict, "conflict")].filter(Boolean).join(" · ");
/** Recent changes, each with a way back to just before it, and any other time. */
export async function rollbackDialog(id) {
  const folder = store.state.sync.folders.find(f => f.id === id);
  const groups = await api(`/api/sync/folders/${enc(id)}/activity`);
  const rows = groups.map(g => ({
    key: `${g.start}-${g.device}`,
    cells: [
      html`${when(g.start)}<small class="muted block">${ago(g.start)}</small>`,
      g.device,
      changeText(g),
      Button({ label: "Just before this", action: "sync-rollback-preview", id, data: { time: g.start - 1 }, size: "sm", kind: "ghost" }),
    ],
  }));
  modal(`Roll back ${folder?.name || "folder"}`, html`<p class="lead">Put every file back the way it was at an earlier time, on every device. What it replaces stays in history, so a rollback can be undone too. You see what would change before anything happens.</p>${DialogSection("Recent changes", Table({ columns: ["When", "Device", "Changes", ""], rows, empty: "Nothing has been replaced or deleted yet." }))}${DialogSection("Another time", Form({ name: "sync-rollback-time", id, body: html`${Field({ name: "when", label: "Date and time", type: "datetime-local", required: true, attrs: { step: 1 }, help: "Changes made in the same second as this time are included." })}${FormFooter(Submit({ label: "Preview", kind: "secondary" }))}` }))}`);
}
const pathList = (title, paths, total) => paths.length ? html`<details class="disclosure"><summary>${title}</summary><ul class="mono">${paths.map(p => html`<li>${p}</li>`)}</ul>${total > paths.length ? html`<p class="help">And ${total - paths.length} more.</p>` : ""}</details>` : "";
/** What the folder held at `time`, and what rolling back would change. */
export async function rollbackPreview(id, time) {
  const s = await api(`/api/sync/folders/${enc(id)}/snapshot?time=${time}`);
  const nothing = !s.restore && !s.remove;
  const body = html`
    <p class="lead">The folder as it was on ${when(time, true)}.</p>
    ${KeyValue([
      ["Files then", `${plural(s.files, "file")} · ${fmt(s.bytes)}`],
      ["Brought back", s.restore ? `${plural(s.restore, "file")} missing or different now` : "Nothing"],
      ["Added since", s.remove ? `${plural(s.remove, "file")}, kept unless you choose below` : "Nothing"],
      ["Unchanged", plural(s.unchanged, "file")],
    ])}
    ${s.unavailable ? Notice({ tone: "warn", body: `${plural(s.unavailable, "file")} changed more often than this folder keeps versions, so what they held then is gone. They stay as they are.` }) : ""}
    ${pathList("Files brought back", s.restore_paths, s.restore)}${pathList("Files added since", s.remove_paths, s.remove)}${pathList("Files with no version from then", s.unavailable_paths, s.unavailable)}
    ${nothing ? Notice({ body: "The folder already matches this time. You can still download it." }) : ""}
    ${Form({ name: "sync-rollback", id, data: { time }, body: html`${s.remove ? Checkbox({ name: "remove_newer", label: `Also delete the ${plural(s.remove, "file")} added since then`, help: "They go to history, like any deletion." }) : ""}${FormFooter([LinkButton({ label: "Download as ZIP", href: archiveUrl(id, time), download: true, icon: "download" }), nothing ? "" : Submit({ label: "Roll back", kind: "danger", icon: "history" })], { align: "between" })}` })}`;
  modal("Roll back preview", body);
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

