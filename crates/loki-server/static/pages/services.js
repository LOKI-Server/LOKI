// The overview, and the three kinds of resource: applications, file shares, and routes.
import { ago, api, enc, fmt, store } from "../lib/core.js";
import { attrs, html } from "../lib/html.js";
import { icon } from "../lib/icons.js";
import { Badge, Button, EmptyState, Hint, IconButton, LinkButton, Meter, Notice, PageHeader, SectionHead, Status } from "../lib/ui.js";

const kinds = { app: "apps", share: "folder", route: "route", sync: "sync" };

/** A route's or share's own icon when it has one, or the icon for its kind. */
export function resourceIcon(r) {
  return r.icon && (r.kind === "route" || r.kind === "share")
    ? html`<img class="resource-picture" src="${`/avatars/services/${encodeURIComponent(r.id)}?v=${store.avatarVersion}`}" alt="" decoding="async" loading="lazy">`
    : icon(kinds[r.kind]);
}
const audiences = { public: "Public", members: "All members", restricted: "Restricted" };

export function resourceStatus(r) {
  if (r.kind === "app") return r.ready ? Status("Running") : r.enabled ? Status("Starting", "live") : Status("Disabled", "neutral");
  if (r.kind === "route") return r.enabled ? Status("Enabled") : Status("Disabled", "neutral");
  return Status(audiences[r.visibility] || r.visibility, r.visibility === "public" ? "ok" : "neutral");
}

// Where a resource opens: a link, or an action for apps managed in a panel.
function opener(r) {
  if (r.kind === "share") return { href: `/shares/${enc(r.id)}` };
  if (r.kind === "route") return { href: r.url, external: true };
  if (r.kind === "app" && r.ready && r.manifest?.site !== false) return { href: r.url, external: true };
  if (r.kind === "app" && r.ready && r.manifest?.panel) return { action: "app-panel" };
  return {};
}

const manage = r => store.session.user.owner ? IconButton({ icon: "settings", label: `Manage ${r.name}`, action: `${r.kind}-manage`, id: r.id, size: "sm" }) : "";

/** A full card for one resource, used on the Applications, File shares and Routes pages. */
export function resourceCard(r) {
  const description = r.kind === "app" ? r.manifest.description : r.kind === "share" ? (r.share_type === "apps" ? "Desktop builds people can download and update with LOKI Client." : "Browse, preview, and download shared files.") : r.hostname || r.url;
  const open = opener(r);
  const primary = open.href ? LinkButton({ label: r.kind === "share" ? "Browse files" : "Open", href: open.href, external: open.external, icon: r.kind === "share" ? "folder" : "external", size: "sm" })
    : open.action ? Button({ label: "Open panel", action: open.action, id: r.id, icon: "apps", size: "sm" }) : "";
  const panel = r.kind === "app" && open.href && r.manifest?.panel ? Button({ label: "Panel", action: "app-panel", id: r.id, icon: "apps", size: "sm", kind: "ghost" }) : "";
  const update = r.update ? Button({ label: `Update to v${r.update}`, action: "registry-install", id: r.id, icon: "refresh", size: "sm", kind: "primary" }) : "";
  return html`<article class="${`resource-card kind-${r.kind}`}" data-key="${r.id}"><header class="resource-head"><span class="${r.icon ? "resource-icon has-picture" : "resource-icon"}" aria-hidden="true">${resourceIcon(r)}</span><div class="resource-title"><h3>${r.name}</h3><div class="resource-tags">${r.kind === "share" ? Badge(r.share_type === "apps" ? "App repository" : "File server", "neutral") : ""}${r.kind === "app" ? Badge(`v${r.manifest.version}`, "neutral") : ""}${r.update ? Badge("Update available", "info") : ""}</div></div><div class="resource-end">${resourceStatus(r)}${manage(r)}</div></header><p class="resource-text">${description}</p>${r.error ? Notice({ tone: "danger", body: r.error }) : ""}<footer class="resource-actions">${update}${primary}${panel}</footer></article>`;
}

export const resourceGrid = items => html`<div class="card-grid">${items.map(resourceCard)}</div>`;

/** A compact, clickable row for one resource, used on the overview. */
function tile(r) {
  const open = opener(r);
  const subtitle = r.kind === "share" ? `${r.share_type === "apps" ? "App repository" : "File server"} · ${audiences[r.visibility] || r.visibility}` : r.kind === "route" ? r.hostname || r.url : r.manifest?.description || "";
  const name = open.href ? html`<a class="tile-title" href="${open.href}"${open.external ? html` target="_blank" rel="noopener noreferrer"` : ""}>${r.name}${open.external ? icon("external", "tile-external") : ""}</a>`
    : open.action ? html`<button type="button" class="tile-title link-button" data-action="${open.action}" data-id="${r.id}">${r.name}</button>`
    : html`<span class="tile-title">${r.name}</span>`;
  return html`<article class="${`tile kind-${r.kind}`}" data-key="${r.id}"><span class="${r.icon ? "tile-icon has-picture" : "tile-icon"}" aria-hidden="true">${resourceIcon(r)}</span><div class="tile-body">${name}<span class="tile-sub">${r.error || subtitle}</span></div><div class="tile-end">${r.update ? Badge("Update", "info") : ""}${r.kind === "share" ? "" : resourceStatus(r)}${manage(r)}</div></article>`;
}
const tiles = items => html`<div class="tiles">${items.map(tile)}</div>`;

// The features LOKI itself provides, shown above the things people add to it.
function features(repos) {
  const { session, state } = store, owner = session.user.owner;
  const syncResource = state.resources.find(r => r.kind === "sync");
  const cards = [];
  if (state.sync || (owner && syncResource)) {
    const sync = state.sync, conflicts = sync ? sync.folders.reduce((n, f) => n + f.conflicts, 0) : 0;
    cards.push(feature({
      kind: "sync", icon: "sync", title: "Save sync", href: "/sync", subtitle: "Game saves and files, the same on every device",
      badge: conflicts ? Badge(`${conflicts} conflict${conflicts === 1 ? "" : "s"}`, "warn") : "",
      body: sync ? html`<p>${sync.folders.length} folder${sync.folders.length === 1 ? "" : "s"} · ${fmt(sync.usage)} of ${fmt(sync.quota)}</p>${Meter({ value: sync.usage, max: sync.quota, label: "Save sync storage used" })}` : html`<p>Sync is off for your account. Give yourself access to use it.</p>`,
      actions: [LinkButton({ label: "Open", href: "/sync", icon: "sync", size: "sm" }), owner && syncResource ? Button({ label: "Manage", action: "sync-manage", id: syncResource.id, icon: "settings", size: "sm", kind: "ghost" }) : ""],
    }));
  }
  if (repos) {
    const count = repos.repositories.length;
    cards.push(feature({
      kind: "repos", icon: "repo", title: "Repositories", href: "/repositories", subtitle: "Version control with Skald",
      body: html`<p>${count ? `${count} repositor${count === 1 ? "y" : "ies"} you can open` : "No repositories yet"}</p>`,
      actions: [LinkButton({ label: "Open", href: "/repositories", icon: "repo", size: "sm" }), repos.create ? Button({ label: "New", action: "vcs-new", icon: "plus", size: "sm", kind: "ghost" }) : "", owner ? Button({ label: "Manage", action: "vcs-manage", icon: "settings", size: "sm", kind: "ghost" }) : ""],
    }));
  }
  return cards.length ? html`<div class="features">${cards}</div>` : "";
}
const feature = f => html`<article class="${`feature kind-${f.kind}`}"><header class="feature-head"><span class="resource-icon" aria-hidden="true">${icon(f.icon)}</span><div><h3><a href="${f.href}">${f.title}</a></h3><span class="tile-sub">${f.subtitle}</span></div>${f.badge}</header>${f.body}<footer class="feature-actions">${f.actions}</footer></article>`;

/**
 * What takes up space in LOKI's data folder, for the owner. The server measures it
 * when asked and keeps the result for a while, so opening the Overview is cheap.
 */
function storageSection(usage) {
  if (!usage) return "";
  const total = Math.max(1, usage.total);
  // An SVG bar, so its widths are attributes rather than inline styles.
  let x = 0;
  const bar = html`<svg class="usage-bar" viewBox="0 0 1000 12" preserveAspectRatio="none" role="img" aria-label="${`${fmt(usage.total)} used, by kind`}">${usage.items.map((item, i) => {
    const width = Math.max(item.bytes / total * 1000, 2);
    const part = html`<rect${attrs({ x: x.toFixed(1), y: 0, width: width.toFixed(1), height: 12, class: `usage-${i % 11}` })}><title>${item.label}: ${fmt(item.bytes)}</title></rect>`;
    x += width;
    return part;
  })}</svg>`;
  const legend = html`<ul class="usage-legend">${usage.items.map((item, i) => html`<li><span class="${`usage-dot usage-${i % 11}`}"></span><span class="usage-label">${item.label}</span><span class="usage-size">${fmt(item.bytes)}</span><span class="usage-share">${Math.round(item.bytes / total * 100) || "<1"}%</span></li>`)}</ul>`;
  const disk = usage.disk?.free != null && usage.disk?.total ? `${fmt(usage.disk.free)} free of ${fmt(usage.disk.total)} on this disk` : "";
  return html`${SectionHead({ title: "Storage", count: `${fmt(usage.total)} used`, actions: Button({ label: "Measure again", action: "storage-refresh", icon: "refresh", size: "sm", kind: "ghost" }) })}<div class="card usage-card"><div class="card-body">${bar}${legend}<p class="help">Everything in LOKI's data folder. Measured ${ago(usage.measured)}${disk ? html` · ${disk}` : ""}.</p></div></div>`;
}

export async function overview() {
  const { session, state } = store, owner = session.user.owner;
  const [repos, usage] = await Promise.all([
    api("/api/repos").catch(() => null),
    owner ? api("/api/storage-usage").catch(() => null) : null,
  ]);
  const by = kind => state.resources.filter(r => r.kind === kind);
  const apps = by("app"), routes = by("route"), shares = by("share");
  const pending = state.users?.filter(u => u.status === "pending").length || 0;
  const running = apps.filter(a => a.ready).length;
  const groups = [
    (apps.length || owner) && html`${SectionHead({ title: "Applications", count: apps.length ? `${running} of ${apps.length} running` : undefined, href: "/apps", linkLabel: owner ? "Manage" : "View all" })}${apps.length ? tiles(apps) : Hint({ text: "Install your first app from the registry.", action: LinkButton({ label: "Browse apps", href: "/apps/browse", size: "sm" }) })}`,
    (routes.length || owner) && html`${SectionHead({ title: "Connected services", count: routes.length || undefined, href: owner ? "/routes" : "", linkLabel: "Manage" })}${routes.length ? tiles(routes) : Hint({ text: "Give Jellyfin or another service you already run a place here.", action: Button({ label: "Add route", action: "route-new", size: "sm" }) })}`,
    (shares.length || owner) && html`${SectionHead({ title: "File shares", count: shares.length || undefined, href: "/shares", linkLabel: owner ? "Manage" : "View all" })}${shares.length ? tiles(shares) : Hint({ text: "Share a folder on this server.", action: Button({ label: "Share a folder", action: "share-new", size: "sm" }) })}`,
  ];
  const body = html`${features(repos)}${groups.map(g => g ? html`<section class="page-section">${g}</section>` : "")}${usage ? html`<section class="page-section">${storageSection(usage)}</section>` : ""}`;
  return html`${PageHeader({ title: `Welcome, ${session.user.display_name || session.user.username}.`, description: "A place for everything you bring to life.", realm: "overview" })}${pending ? Notice({ tone: "warn", icon: "people", body: `${pending} account request${pending === 1 ? "" : "s"} waiting.`, action: html`<a href="/accounts">Review requests${icon("chevron")}</a>` }) : ""}${String(body).trim() ? body : EmptyState({ title: "Every universe starts somewhere.", text: "Your server owner can give you access to apps and file shares.", rune: "ᛟ" })}`;
}

export function appsPage() {
  const items = store.state.resources.filter(r => r.kind === "app"), owner = store.session.user.owner;
  const add = owner ? [Button({ label: "Import package", action: "app-import", icon: "upload" }), LinkButton({ label: "Browse apps", href: "/apps/browse", kind: "primary", icon: "apps" })] : "";
  return html`${PageHeader({ title: "Applications", description: "Independent services. One place to keep them together.", actions: add, realm: "apps" })}${items.length ? resourceGrid(items) : EmptyState({ title: "Bring a new branch to life.", text: owner ? "Install a reviewed app from the LOKI app registry, or import a .loki-app package." : "No applications have been shared with you yet.", action: add, rune: "ᚲ" })}`;
}

export function sharesPage() {
  const items = store.state.resources.filter(r => r.kind === "share"), owner = store.session.user.owner;
  const add = owner ? Button({ label: "Share a folder", action: "share-new", kind: "primary", icon: "plus" }) : "";
  return html`${PageHeader({ title: "File shares", description: "A window into the things you want to share.", actions: add, realm: "shares" })}${items.length ? resourceGrid(items) : EmptyState({ title: "A place for your files.", text: owner ? "Select a folder on this server. Files stay where they are and appear as you add them." : "No file shares have been shared with you yet.", action: add, rune: "ᚠ" })}`;
}

export function routesPage() {
  const items = store.state.resources.filter(r => r.kind === "route");
  const add = Button({ label: "Add route", action: "route-new", kind: "primary", icon: "plus" });
  return html`${PageHeader({ title: "Routes & services", description: "Give Jellyfin and your other services a place in your universe.", actions: add, realm: "routes" })}${items.length ? resourceGrid(items) : EmptyState({ title: "Connect what you already run.", text: "Set a hostname and upstream address. LOKI will configure Caddy for you.", action: add, rune: "ᚱ" })}`;
}
