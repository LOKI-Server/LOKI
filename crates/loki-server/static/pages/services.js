// Overview and the three kinds of resource: applications, file shares, and routes.
import { enc, esc, store } from "../lib/core.js";
import { icon } from "../lib/icons.js";
import { button, empty, heading, linkButton, status } from "../lib/ui.js";

const kinds = { app: "apps", share: "folder", route: "route", sync: "sync" };

export function resourceStatus(r) {
  if (r.kind === "app") return r.ready ? status("Running") : r.enabled ? status("Starting", "live") : status("Disabled", "dim");
  if (r.kind === "route") return r.enabled ? status("Enabled") : status("Disabled", "dim");
  return status({ public: "Public", members: "Members", restricted: "Restricted" }[r.visibility] || r.visibility, r.visibility === "public" ? "" : "dim");
}

export function cards(items) {
  const owner = store.session.user.owner;
  return `<div class="grid">${items.map(r => {
    const description = r.kind === "app" ? r.manifest.description : r.kind === "share" ? "Browse, preview, and download shared files." : r.kind === "sync" ? "Keep game saves and files the same on every device." : r.hostname || r.url;
    const open = r.kind === "share" ? linkButton("Browse files", `/shares/${enc(r.id)}`, "subtle", "folder")
      : r.kind === "sync" ? linkButton("Open sync", "/sync", "subtle", "sync")
      : r.kind === "route" || (r.ready && r.manifest?.site !== false) ? linkButton("Open service", r.url, "subtle", "external", 'target="_blank" rel="noopener"') : "";
    // Apps that describe a panel are managed here, inside LOKI.
    const panel = r.kind === "app" && r.ready && r.manifest.panel ? button("Open panel", "app-panel", r.id, "", "subtle", "apps") : "";
    const manage = owner ? button("Manage", `${r.kind}-manage`, r.id, "", "ghost", "settings") : "";
    return `<article class="card resource ${r.kind}" data-key="${esc(r.id)}"><div class="card-header"><span class="card-symbol" aria-hidden="true">${icon(kinds[r.kind])}</span>${resourceStatus(r)}</div><h2>${esc(r.name)}</h2><p>${esc(description)}</p>${r.error ? `<p class="help error-text">${icon("alert")}${esc(r.error)}</p>` : ""}<div class="actions">${panel}${open}${manage}</div></article>`;
  }).join("")}</div>`;
}

function stat(value, total, label, iconName, href) {
  return `<a class="stat" href="${href}"><span class="stat-icon" aria-hidden="true">${icon(iconName)}</span><span><strong>${value}${total !== undefined ? `<small> / ${total}</small>` : ""}</strong><span>${esc(label)}</span></span></a>`;
}

export function overview() {
  const { session, state } = store;
  const apps = state.resources.filter(r => r.kind === "app"), shares = state.resources.filter(r => r.kind === "share"), routes = state.resources.filter(r => r.kind === "route");
  const pending = state.users?.filter(u => u.status === "pending").length || 0;
  const owner = session.user.owner;
  return heading(`Welcome, ${session.user.display_name || session.user.username}.`, "A place for everything you bring to life.", "", "overview")
    + `<div class="stats">${stat(apps.filter(a => a.ready).length, apps.length, "Applications running", "apps", "/apps")}${stat(shares.length, undefined, "File shares", "folder", "/shares")}${stat(routes.filter(r => r.enabled).length, owner ? routes.length : undefined, "Connected routes", "route", owner ? "/routes" : "/")}</div>`
    + (pending ? `<div class="pending-banner">${icon("people")}<span>${pending} account request${pending === 1 ? "" : "s"} waiting.</span><a href="/accounts">Review requests ${icon("chevron")}</a></div>` : "")
    + `<div class="section-heading"><h2>Your services</h2>${owner ? `<a href="/apps">Manage applications ${icon("chevron")}</a>` : ""}</div>`
    + (state.resources.length ? cards(state.resources) : empty("Every universe starts somewhere.", owner ? "Import your first app, connect an existing service, or share a folder." : "Your server owner can give you access to apps and file shares.", owner ? linkButton("Add your first app", "/apps", "primary", "plus") : "", "ᛟ"));
}

export function appsPage() {
  const items = store.state.resources.filter(r => r.kind === "app"), owner = store.session.user.owner;
  return heading("Applications", "Independent services. One place to keep them together.", owner ? button("Import app package", "app-import", "", "", "primary", "upload") : "", "apps")
    + (items.length ? cards(items) : empty("Bring a new branch to life.", owner ? "Import a portable WASI .loki-app package. Bigshot Voter is the first LOKI component." : "No applications have been shared with you yet.", owner ? button("Import app package", "app-import", "", "", "primary", "upload") : "", "ᚲ"));
}

export function sharesPage() {
  const items = store.state.resources.filter(r => r.kind === "share"), owner = store.session.user.owner;
  return heading("File shares", "A window into the things you want to share.", owner ? button("Share a folder", "share-new", "", "", "primary", "plus") : "", "shares")
    + (items.length ? cards(items) : empty("A place for your files.", owner ? "Select a folder on this server. Files stay where they are and appear as you add them." : "No file shares have been shared with you yet.", owner ? button("Share a folder", "share-new", "", "", "primary", "plus") : "", "ᚠ"));
}

export function routesPage() {
  const items = store.state.resources.filter(r => r.kind === "route");
  return heading("Routes & services", "Give Jellyfin and your other services a place in your universe.", button("Add route", "route-new", "", "", "primary", "plus"), "routes")
    + (items.length ? cards(items) : empty("Connect what you already run.", "Set a hostname and upstream address. LOKI will configure Caddy for you.", button("Add route", "route-new", "", "", "primary", "plus"), "ᚱ"));
}
