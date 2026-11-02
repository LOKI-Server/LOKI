// Apps from the LOKI app registry. LOKI's server reads the registry and checks
// every download against the reviewed file, so this page never contacts it directly.
import { api, enc, esc, fmt, modal } from "../lib/core.js";
import { icon } from "../lib/icons.js";
import { button, heading, linkButton, status, tag } from "../lib/ui.js";

const FRESH_MS = 60000;
let cached = null, loadedAt = 0, query = "";

/** The registry list, fetched again after a minute or when `force` is set. */
export async function registry(force = false) {
  if (force || !cached || Date.now() - loadedAt > FRESH_MS) {
    cached = await api(`/api/registry${force ? "?refresh=true" : ""}`);
    loadedAt = Date.now();
  }
  return cached;
}
/** Forget the list, so the next render shows what was just installed. */
export const forget = () => { cached = null; };

const searchText = a => `${a.name} ${a.id} ${a.description}`.toLowerCase();
const matches = a => !query || searchText(a).includes(query);

/** Filters the cards as people type, without asking the server again. */
export function search(input) {
  query = input.value.trim().toLowerCase();
  for (const card of document.querySelectorAll(".registry-app")) card.hidden = !card.dataset.search.includes(query);
  const none = document.getElementById("registry-none");
  if (none) none.hidden = [...document.querySelectorAll(".registry-app")].some(c => !c.hidden);
}

const source = a => a.source ? linkButton("Source", a.source, "subtle", "external", 'target="_blank" rel="noopener noreferrer"') : "";
const appIcon = a => `<span class="card-symbol" aria-hidden="true">${a.icon ? `<img src="/api/registry/icons/${enc(a.id)}" alt="" loading="lazy">` : icon("apps")}</span>`;

function state(a) {
  if (a.status === "installed") return status("Installed");
  if (a.status === "update") return tag(`Update to v${a.version}`);
  if (a.status === "replace") return tag("Uploaded copy installed", "warn");
  if (a.status === "unsupported") return tag("Needs a newer LOKI", "bad");
  return "";
}

function action(a) {
  if (a.status === "install") return button("Install", "registry-install", a.id, "", "primary", "download");
  if (a.status === "update") return button(`Update to v${a.version}`, "registry-install", a.id, "", "primary", "refresh");
  if (a.status === "replace") return button("Use the registry version", "registry-install", a.id, "", "subtle", "download");
  return "";
}

function card(a) {
  const note = a.status === "unsupported" ? a.reason : a.status === "replace" ? `An uploaded v${a.installed} uses this app ID.` : "";
  return `<article class="card resource registry-app" data-key="${esc(a.id)}" data-search="${esc(searchText(a))}"${matches(a) ? "" : " hidden"}>`
    + `<div class="card-header">${appIcon(a)}${state(a)}</div><h2>${esc(a.name)}</h2>`
    + `<div class="tags">${tag(`v${a.version}`, "dim")}${a.capabilities.map(c => tag(c, "dim")).join("")}</div>`
    + `<p>${esc(a.description || "No description.")}</p>${note ? `<p class="help">${icon("alert")}${esc(note)}</p>` : ""}`
    + `<div class="actions">${action(a)}${source(a)}</div></article>`;
}

export async function marketplacePage() {
  const actions = button("Refresh", "registry-refresh", "", "", "subtle", "refresh") + linkButton("Applications", "/apps", "subtle", "left");
  const top = heading("Browse apps", "Reviewed apps from the LOKI app registry, ready to install.", actions, "apps");
  let list;
  try {
    list = await registry();
  } catch (error) {
    return top + `<div class="notice error">${esc(error.message)}</div><p class="help">The registry address is in <a href="/settings">Server settings</a>. You can still import a .loki-app package from Applications.</p>`;
  }
  const stale = list.error ? `<div class="notice">Showing the list LOKI read earlier. ${esc(list.error)}</div>` : "";
  const apps = list.apps;
  const shown = apps.filter(matches).length;
  return top + stale
    + `<div class="toolbar"><input type="search" class="registry-search" data-registry-search placeholder="Search apps" aria-label="Search apps" value="${esc(query)}"><div class="toolbar-end"><span class="muted">${apps.length} app${apps.length === 1 ? "" : "s"}</span></div></div>`
    + `<div class="grid">${apps.map(card).join("")}</div>`
    + `<p id="registry-none" class="help"${shown || !apps.length ? " hidden" : ""}>No apps match your search.</p>`
    + (apps.length ? "" : '<p class="help">The registry has no apps yet.</p>');
}

/** Says what installing does, before anything is downloaded. */
export async function installDialog(id) {
  const a = (await registry()).apps.find(x => x.id === id);
  if (!a) throw new Error("The app registry no longer lists this app.");
  const verb = a.status === "install" ? "Install" : "Update";
  const row = (label, value) => `<div class="kv-row"><dt>${esc(label)}</dt><dd>${value}</dd></div>`;
  modal(`${verb} ${a.name}`, `<div class="package-preview">${a.description ? `<p>${esc(a.description)}</p>` : ""}`
    + `<dl class="kv">${row("Version", a.installed ? `v${esc(a.installed)} → v${esc(a.version)}` : `v${esc(a.version)}`)}${row("Capabilities", a.capabilities.length ? `<div class="tags">${a.capabilities.map(c => tag(c)).join("")}</div>` : '<span class="kv-none">None</span>')}${row("Download", esc(fmt(a.size)))}${row("Built with", `loki-sdk ${esc(a.sdk)}`)}${a.source ? row("Source", `<a href="${esc(a.source)}" target="_blank" rel="noopener noreferrer">${esc(a.source)}</a>`) : ""}</dl></div>`
    + `<div class="notice">Apps are reviewed before they're listed, and LOKI installs only the exact file that was reviewed. Look at the source if you're unsure what an app does.</div>`
    + (a.installed ? '<p class="help">LOKI stops the installed version and snapshots its data before updating.</p>' : "")
    + `<div class="actions">${button(`${verb} ${a.name}`, "registry-confirm", a.id, "", "primary", "download")}</div>`);
}
