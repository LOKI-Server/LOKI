// Apps from the LOKI app registry. LOKI's server reads the registry and checks
// every download against the reviewed file, so this page never contacts it directly.
import { api, enc, fmt, modal } from "../lib/core.js";
import { html, plural } from "../lib/html.js";
import { icon } from "../lib/icons.js";
import { Badge, Button, FormFooter, Help, KeyValue, LinkButton, Notice, PageHeader, Status, Toolbar } from "../lib/ui.js";

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

const source = a => a.source ? LinkButton({ label: "Source", href: a.source, external: true, icon: "external", size: "sm", kind: "ghost" }) : "";
const appIcon = a => html`<span class="resource-icon" aria-hidden="true">${a.icon ? html`<img src="${`/api/registry/icons/${enc(a.id)}`}" alt="" loading="lazy">` : icon("apps")}</span>`;

function state(a) {
  if (a.status === "installed") return Status("Installed");
  if (a.status === "update") return Badge(`Update to v${a.version}`, "info");
  if (a.status === "replace") return Badge("Uploaded copy installed", "warn");
  if (a.status === "unsupported") return Badge("Needs a newer LOKI", "danger");
  return "";
}

function action(a) {
  if (a.status === "install") return Button({ label: "Install", action: "registry-install", id: a.id, kind: "primary", icon: "download", size: "sm" });
  if (a.status === "update") return Button({ label: `Update to v${a.version}`, action: "registry-install", id: a.id, kind: "primary", icon: "refresh", size: "sm" });
  if (a.status === "replace") return Button({ label: "Use the registry version", action: "registry-install", id: a.id, icon: "download", size: "sm" });
  return "";
}

function card(a) {
  const note = a.status === "unsupported" ? a.reason : a.status === "replace" ? `An uploaded v${a.installed} uses this app ID.` : "";
  return html`<article class="resource-card registry-app" data-key="${a.id}" data-search="${searchText(a)}"${matches(a) ? "" : html` hidden`}><header class="resource-head">${appIcon(a)}<div class="resource-title"><h3>${a.name}</h3><div class="resource-tags">${Badge(`v${a.version}`, "neutral")}${a.capabilities.map(c => Badge(c, "neutral"))}</div></div>${state(a)}</header><p class="resource-text">${a.description || "No description."}</p>${note ? Notice({ tone: a.status === "unsupported" ? "danger" : "warn", body: note }) : ""}<footer class="resource-actions">${action(a)}${source(a)}</footer></article>`;
}

export async function marketplacePage() {
  const actions = [LinkButton({ label: "Applications", href: "/apps", icon: "left" }), Button({ label: "Refresh", action: "registry-refresh", icon: "refresh" })];
  const top = PageHeader({ title: "Browse apps", description: "Reviewed apps from the LOKI app registry, ready to install.", actions, realm: "apps" });
  let list;
  try {
    list = await registry();
  } catch (error) {
    return String(html`${top}${Notice({ tone: "danger", body: error.message })}<p class="help">The registry address is in <a href="/settings#registry">Server settings</a>. You can still import a .loki-app package from Applications.</p>`);
  }
  const apps = list.apps;
  const shown = apps.filter(matches).length;
  const search = html`<input type="search" class="search-input" data-registry-search placeholder="Search apps" aria-label="Search apps" value="${query}">`;
  return String(html`${top}${list.error ? Notice({ tone: "warn", body: `Showing the list LOKI read earlier. ${list.error}` }) : ""}${Toolbar({ start: search, end: html`<span class="muted">${plural(apps.length, "app")}</span>` })}<div class="card-grid">${apps.map(card)}</div><p id="registry-none" class="help"${shown || !apps.length ? html` hidden` : ""}>No apps match your search.</p>${apps.length ? "" : Help("The registry has no apps yet.")}`);
}

/** Says what installing does, before anything is downloaded. */
export async function installDialog(id) {
  const a = (await registry()).apps.find(x => x.id === id);
  if (!a) throw new Error("The app registry no longer lists this app.");
  const verb = a.status === "install" ? "Install" : "Update";
  modal(`${verb} ${a.name}`, html`<div class="package-preview">${a.description ? html`<p>${a.description}</p>` : ""}${KeyValue([
    ["Version", a.installed ? `v${a.installed} → v${a.version}` : `v${a.version}`],
    ["Capabilities", a.capabilities.length ? html`<div class="badge-row">${a.capabilities.map(c => Badge(c))}</div>` : ""],
    ["Download", fmt(a.size)],
    ["Built with", `loki-sdk ${a.sdk}`],
    a.source && ["Source", LinkButton({ label: a.source, href: a.source, external: true, kind: "ghost", size: "sm", className: "btn-link" })],
  ])}</div>${Notice({ tone: "info", body: "Apps are reviewed before they're listed, and LOKI installs only the exact file that was reviewed. Look at the source if you're unsure what an app does." })}${a.installed ? Help("LOKI stops the installed version and snapshots its data before updating.") : ""}${FormFooter(Button({ label: `${verb} ${a.name}`, action: "registry-confirm", id: a.id, kind: "primary", icon: "download" }), { divider: false })}`);
}
