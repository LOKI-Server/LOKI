// Client-side navigation. Pages render from the state already in memory, so moving
// between them needs no request; stale state is refreshed afterwards and patched in.
import { $, api, enc, esc, morph, reducedMotion, store } from "./core.js";
import { icon } from "./icons.js";
import { button, heading, skeleton, tag } from "./ui.js";
import { authPage, signOutPage } from "../pages/auth.js";
import { appsPage, overview, routesPage, sharesPage } from "../pages/services.js";
import { accountsPage, activityPage, avatar } from "../pages/people.js";
import { accountPage, settingsPage } from "../pages/account.js";
import { browse } from "../pages/files.js";
import { folderPage, pairPage, syncPage } from "../pages/sync.js";

const memberPages = { "/": overview, "/apps": appsPage, "/shares": sharesPage, "/sync": syncPage, "/pair": pairPage, "/account": accountPage };
const ownerPages = { "/routes": routesPage, "/accounts": accountsPage, "/activity": activityPage, "/settings": settingsPage };
const STALE_MS = 15000;

export async function refreshState({ session = false } = {}) {
  const [nextSession, state] = await Promise.all([session ? api("/api/session") : store.session, api("/api/state")]);
  store.session = nextSession;
  store.state = state;
  store.loadedAt = Date.now();
}

function browseParams() {
  const query = new URLSearchParams(location.search);
  return { path: query.get("path") || "", page: Number(query.get("page")) || 0, sort: query.get("sort") || "name" };
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
  if (session.user.status !== "approved") return heading("Your account is waiting", "The server owner will review your account request.", "", "waiting") + `<div class="card narrow"><p>Status: ${tag(session.user.status, "warn")}</p>${button("Sign out", "logout", "", "", "subtle", "logout")}</div>`;
  if (page.startsWith("/shares/")) return browse(decodeURIComponent(page.split("/")[2]), browseParams());
  if (page.startsWith("/sync/")) return folderPage(decodeURIComponent(page.split("/")[2]), browseParams());
  const view = memberPages[page] || (session.user.owner && ownerPages[page]) || overview;
  return view();
}

export function nav() {
  const { session, state } = store, user = session.user;
  const here = location.pathname;
  const link = (path, label, iconName, badge = 0) => {
    const active = path === "/" ? here === "/" : here === path || here.startsWith(`${path}/`);
    return `<a href="${path}"${active ? ' class="active" aria-current="page"' : ""}>${icon(iconName)}<span>${label}</span>${badge ? `<span class="badge" aria-label="${badge} waiting">${badge}</span>` : ""}</a>`;
  };
  let links = "";
  if (user && user.status === "approved") {
    links = `<div class="nav-section"><span class="section-label">Your universe</span>${link("/", "Overview", "home")}${link("/apps", "Applications", "apps")}${link("/shares", "File shares", "folder")}${state?.sync ? link("/sync", "Save sync", "sync", state.sync.folders.reduce((n, f) => n + f.conflicts, 0)) : ""}</div>`;
    if (user.owner) {
      const pending = state?.users?.filter(u => u.status === "pending").length || 0;
      links += `<div class="nav-section"><span class="section-label">Server</span>${link("/routes", "Routes & services", "route")}${link("/accounts", "People & groups", "people", pending)}${link("/activity", "Activity", "activity")}${link("/settings", "Server settings", "settings")}</div>`;
    }
    links += `<div class="nav-section"><span class="section-label">You</span>${link("/account", "My account", "user")}</div>`;
  }
  morph($("#navigation"), links);
  $("#server-name").innerHTML = `<span class="status-dot"></span>${esc(session.name || "LOKI Server")}`;
  morph($("#identity"), user ? `<a class="account" href="/account">${avatar(user)}<span class="username">${esc(user.display_name || user.username)}</span>${user.owner ? tag("Owner") : ""}</a>${button("Sign out", "logout", "", 'aria-label="Sign out" title="Sign out"', "icon-button", "logout")}` : '<a class="button subtle" href="/login">Sign in</a>');
}

export function showError(error) {
  $("#main").innerHTML = heading("The branch is out of reach.", error.message, button("Try again", "reload", "", "", "primary", "refresh"), "lost");
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
    main.innerHTML = skeleton();
  }, 120);
  let html;
  try { html = await resolve(); } finally { clearTimeout(placeholder); }
  if (token !== renderToken) return;
  const apply = () => {
    if (keep) morph(main, html);
    else main.innerHTML = html;
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
    scrollTo({ top: 0 });
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
