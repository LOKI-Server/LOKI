// Client-side navigation. Pages render from the state already in memory, so moving
// between them needs no request; stale state is refreshed afterwards and patched in.
import { $, api, enc, morph, reducedMotion, store } from "./core.js";
import { html } from "./html.js";
import { icon } from "./icons.js";
import { Badge, Button, Card, IconButton, LinkButton, PageHeader, Skeleton } from "./ui.js";
import { authPage, signOutPage } from "../pages/auth.js";
import { appsPage, overview, routesPage, sharesPage } from "../pages/services.js";
import { accountsPage, activityPage, avatar } from "../pages/people.js";
import { accountPage, exportPage, settingsPage, pairPage, vcsCredentialsPage } from "../pages/account.js";
import { browse } from "../pages/files.js";
import { marketplacePage } from "../pages/marketplace.js";
import { folderPage, syncPage } from "../pages/sync.js";
import { vcsPage, vcsRepo, vcsItem } from "../pages/vcs.js";

const memberPages = { "/": overview, "/apps": appsPage, "/shares": sharesPage, "/sync": syncPage, "/pair": pairPage, "/account": accountPage, "/account/export": exportPage, "/account/vcs-credentials": vcsCredentialsPage, "/repositories": vcsPage };
const ownerPages = { "/apps/browse": marketplacePage, "/routes": routesPage, "/accounts": accountsPage, "/activity": activityPage, "/settings": settingsPage };
const STALE_MS = 15000;

export async function refreshState({ session = false } = {}) {
  const [nextSession, state] = await Promise.all([session ? api("/api/session") : store.session, api("/api/state")]);
  store.session = nextSession;
  store.state = state;
  store.loadedAt = Date.now();
}

function browseParams() {
  const query = new URLSearchParams(location.search);
  return { path: query.get("path") || "", page: Number(query.get("page")) || 0, sort: query.get("sort") || "name", deleted: query.get("deleted") === "1" };
}

async function resolve() {
  const { session } = store, page = location.pathname;
  if (session.setup) return authPage("setup");
  if (page === "/reset") return authPage("reset");
  if (page === "/sign-out") return signOutPage();
  if (!session.user) {
    if (page.startsWith("/shares/")) {
      const id = decodeURIComponent(page.split("/")[2]);
      try {
        const share = await api(`/api/shares/${enc(id)}/info`);
        store.state = { resources: [share] };
        return await browse(id, browseParams());
      } catch {}
    }
    return authPage(page === "/register" ? "register" : "login");
  }
  if (!store.state) await refreshState();
  if (session.user.status !== "approved") return html`${PageHeader({ title: "Your account is waiting", description: "The server owner will review your account request.", realm: "waiting" })}${Card({ className: "narrow", body: html`<p>Status: ${Badge(session.user.status, "warn")}</p>${Button({ label: "Sign out", action: "logout", icon: "logout" })}` })}`;
  if (page.startsWith("/shares/")) return browse(decodeURIComponent(page.split("/")[2]), browseParams());
  if (page.startsWith("/sync/")) return folderPage(decodeURIComponent(page.split("/")[2]), browseParams());
  const repo = /^\/repositories\/([^/]+)(?:\/items\/([^/]+))?$/.exec(page);
  if (repo) return repo[2] ? vcsItem(decodeURIComponent(repo[1]), decodeURIComponent(repo[2])) : vcsRepo(decodeURIComponent(repo[1]));
  const view = memberPages[page] || (session.user.owner && ownerPages[page]) || overview;
  return view();
}

export function nav() {
  const { session, state } = store, user = session.user;
  const here = location.pathname;
  const link = (path, label, iconName, badge = 0) => {
    const active = path === "/" ? here === "/" : here === path || here.startsWith(`${path}/`);
    return html`<a href="${path}"${active ? html` class="active" aria-current="page"` : ""}>${icon(iconName)}<span>${label}</span>${badge ? html`<span class="nav-badge" aria-label="${badge} waiting">${badge}</span>` : ""}</a>`;
  };
  const section = (label, links) => html`<div class="nav-section"><span class="section-label">${label}</span>${links}</div>`;
  let links = "";
  if (user && user.status === "approved") {
    const conflicts = state?.sync ? state.sync.folders.reduce((n, f) => n + f.conflicts, 0) : 0;
    const pending = state?.users?.filter(u => u.status === "pending").length || 0;
    links = html`${section("Your universe", [link("/", "Overview", "home"), link("/apps", "Applications", "apps"), link("/shares", "File shares", "folder"), link("/repositories", "Repositories", "repo", state?.transfers_waiting || 0), state?.sync ? link("/sync", "Save sync", "sync", conflicts) : ""])}${user.owner ? section("Server", [link("/routes", "Routes & services", "route"), link("/accounts", "People & groups", "people", pending), link("/activity", "Activity", "activity"), link("/settings", "Server settings", "settings")]) : ""}${section("You", link("/account", "My account", "user"))}`;
  }
  morph($("#navigation"), links);
  $("#server-name").innerHTML = String(html`<span class="status-dot"></span>${session.name || "LOKI Server"}`);
  // The tab is named after the server, and follows a rename without a reload.
  document.title = session.name?.trim() || "LOKI";
  morph($("#identity"), user ? html`<a class="account" href="/account">${avatar(user)}<span class="username">${user.display_name || user.username}</span>${user.owner ? Badge("Owner") : ""}</a>${IconButton({ icon: "logout", label: "Sign out", action: "logout" })}` : LinkButton({ label: "Sign in", href: "/login", size: "sm" }));
}

export function showError(error) {
  $("#main").innerHTML = String(PageHeader({ title: "The branch is out of reach.", description: error.message, actions: Button({ label: "Try again", action: "reload", kind: "primary", icon: "refresh" }), realm: "lost" }));
  $("#main").removeAttribute("aria-busy");
}

let renderToken = 0;
/**
 * Draw the page for the current URL. `keep` patches the existing page in place
 * (for refreshes); otherwise the page is replaced, with a view transition when asked.
 */
export async function render({ transition = false, keep = false } = {}) {
  const token = ++renderToken, main = $("#main");
  // Only show placeholders when the page is actually slow to arrive.
  const placeholder = keep ? 0 : setTimeout(() => {
    if (token !== renderToken) return;
    main.setAttribute("aria-busy", "true");
    main.innerHTML = String(Skeleton());
  }, 120);
  let page;
  try { page = await resolve(); } finally { clearTimeout(placeholder); }
  if (token !== renderToken) return;
  const apply = () => {
    if (keep) morph(main, page);
    else main.innerHTML = String(page);
    main.removeAttribute("aria-busy");
    nav();
  };
  let applied = false;
  const once = () => { if (!applied) { applied = true; apply(); } };
  if (transition && document.startViewTransition && !reducedMotion() && document.visibilityState === "visible") {
    // The animation is a nicety. Browsers may skip or abort it (background tab,
    // rapid clicks), so the page is updated regardless after a short wait.
    const view = document.startViewTransition(once);
    view.ready.catch(() => {});
    view.finished.catch(() => {});
    await Promise.race([view.updateCallbackDone.catch(() => {}), new Promise(done => setTimeout(done, 250))]);
  }
  once();
  if (!keep) {
    const target = location.hash && document.getElementById(decodeURIComponent(location.hash.slice(1)));
    if (target) target.scrollIntoView();
    else scrollTo({ top: 0 });
    main.focus({ preventScroll: true });
  }
  schedulePoll(keep);
}

/** Fetch fresh state and patch it into the current page without losing scroll or input. */
export async function refresh({ session = false } = {}) {
  if (store.session?.user) await refreshState({ session });
  await render({ keep: true });
}

export async function navigate(url, { replace = false } = {}) {
  const target = new URL(url, location.href);
  if (target.pathname + target.search !== location.pathname + location.search) history[replace ? "replaceState" : "pushState"](null, "", target.pathname + target.search + target.hash);
  await render({ transition: true });
  if (store.session?.user && Date.now() - store.loadedAt > STALE_MS) await refresh();
}

/** Links that stay inside the dashboard. Everything else is left to the browser. */
export function routable(link, event) {
  if (event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return false;
  if ((link.target && link.target !== "_self") || link.hasAttribute("download")) return false;
  const url = new URL(link.href, location.href);
  if (url.origin !== location.origin) return false;
  // Jumps within the page are left to the browser.
  if (url.hash && url.pathname === location.pathname && url.search === location.search) return false;
  return !/^\/(api|oidc|avatars|assets|gateway|_loki|\.well-known|sync\/v1)(\/|$)/.test(url.pathname);
}

// While an application is starting, check on it with a gentle back-off.
let pollTimer, pollCount = 0;
function schedulePoll(keep) {
  clearTimeout(pollTimer);
  if (!keep) pollCount = 0;
  const starting = store.session?.user && store.state?.resources?.some(r => r.kind === "app" && r.enabled && !r.ready);
  if (!starting || !["/", "/apps"].includes(location.pathname) || pollCount > 20) return;
  pollTimer = setTimeout(() => {
    pollCount++;
    if (document.hidden) return schedulePoll(true);
    refresh().catch(() => schedulePoll(true));
  }, Math.min(30000, 2000 * 1.4 ** pollCount));
}
