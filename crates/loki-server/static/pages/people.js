// Owner pages for people, groups, and the audit trail.
import { ago, esc, modal, store } from "../lib/core.js";
import { icon } from "../lib/icons.js";
import { button, empty, heading, initial, status, table, tag } from "../lib/ui.js";
import { quotaSection } from "./sync.js";

const statusKind = { approved: "", pending: "warn live", suspended: "bad", rejected: "bad" };
const groupName = id => store.state.groups.find(g => g.id === id)?.name || id;

export function avatar(user) {
  return `<span class="avatar" aria-hidden="true">${user.avatar ? `<img src="/avatars/${encodeURIComponent(user.id)}?v=${store.avatarVersion}" alt="" decoding="async" loading="lazy">` : initial(user.display_name || user.username)}</span>`;
}

export function accountsPage() {
  const { state } = store;
  const pending = state.users.filter(u => u.status === "pending");
  const rows = state.users.map(u => ({
    key: u.id,
    cells: [
      `<div class="person">${avatar(u)}<div><strong>${esc(u.display_name || u.username)}</strong><small>@${esc(u.username)}${u.owner ? " · Owner" : ""}</small></div></div>`,
      status(u.status[0].toUpperCase() + u.status.slice(1), statusKind[u.status] ?? "dim"),
      `<div class="chips">${state.memberships.filter(([uid]) => uid === u.id).map(([, gid]) => tag(groupName(gid), "dim")).join("") || '<span class="muted">—</span>'}</div>`,
      button(u.status === "pending" ? "Review" : "Manage", "user-manage", u.id, "", u.status === "pending" ? "primary" : "subtle"),
    ],
  }));
  return heading("People & groups", "The people who make this universe yours.", button("Create account", "user-new", "", "", "primary", "plus"), "people")
    + (pending.length ? `<div class="pending-banner">${icon("people")}<span>${pending.length} account request${pending.length === 1 ? "" : "s"} waiting for your review.</span></div>` : "")
    + table(["Person", "Status", "Groups", ""], rows)
    + `<div class="section-heading"><h2>Groups</h2>${button("New group", "group-new", "", "", "subtle", "plus")}</div>`
    + `<div class="grid compact">${state.groups.map(g => {
      const members = state.memberships.filter(([, gid]) => gid === g.id).length;
      return `<div class="card group" data-key="${esc(g.id)}"><div class="card-header"><span class="card-symbol" aria-hidden="true">${icon("people")}</span>${g.id !== "members" ? button("Delete group", "group-delete", g.id, 'aria-label="Delete group"', "icon-button", "trash") : tag("Everyone", "dim")}</div><h3>${esc(g.name)}</h3><p>${members} member${members === 1 ? "" : "s"}</p></div>`;
    }).join("")}</div>`;
}

export function userManage(id) {
  const { state } = store;
  const u = state.users.find(u => u.id === id);
  const labels = { approved: "Approve", suspended: "Suspend", rejected: "Reject" };
  const statusButtons = u.owner ? "" : Object.keys(labels).filter(s => s !== u.status).map(s => button(labels[s], "user-status", id, `data-status="${s}"`, s === "approved" ? "primary" : "danger", s === "approved" ? "check" : "close")).join("");
  modal(u.display_name || u.username, `<div class="person large">${avatar(u)}<div><strong>@${esc(u.username)}</strong>${status(u.status[0].toUpperCase() + u.status.slice(1), statusKind[u.status] ?? "dim")}</div></div><div class="actions">${statusButtons}${button("Issue password reset link", "reset-issue", id, "", "subtle", "key")}</div><section class="dialog-section"><h3>Group membership</h3><div class="check-list">${state.groups.map(g => `<label class="check"><input type="checkbox" data-membership="${esc(id)}" data-group="${esc(g.id)}" ${state.memberships.some(([uid, gid]) => uid === id && gid === g.id) ? "checked" : ""}><span>${esc(g.name)}</span></label>`).join("")}</div></section>${quotaSection(id)}`);
}

export function activityPage() {
  const users = Object.fromEntries(store.state.users.map(u => [u.id, u]));
  const rows = store.state.audit.map(a => {
    const who = users[a.actor];
    return {
      cells: [
        ago(a.created),
        who ? `<div class="person small">${avatar(who)}<span>${esc(who.username)}</span></div>` : `<span class="muted">${esc(a.actor || "System")}</span>`,
        `<code class="action-name">${esc(a.action)}</code>`,
        `<span class="detail">${esc(a.detail)}</span>`,
      ],
    };
  });
  return heading("Activity", "The recent story of this server.", "", "activity")
    + (rows.length ? table(["When", "Who", "Action", "Resource"], rows) : empty("Nothing has happened yet.", "Sign-ins, changes, and service events will appear here.", "", "ᛞ"));
}
