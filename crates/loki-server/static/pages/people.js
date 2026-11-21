// Owner pages for people, groups, and the audit trail.
import { ago, modal, store } from "../lib/core.js";
import { attrs, html, plural } from "../lib/html.js";
import { Badge, Button, ButtonGroup, Card, DialogSection, EmptyState, IconButton, Notice, PageHeader, SectionHead, Status, Table, initial } from "../lib/ui.js";
import { quotaSection } from "./sync.js";

const statusTone = { approved: "ok", pending: "live", suspended: "danger", rejected: "danger" };
const statusLabel = s => s[0].toUpperCase() + s.slice(1);
const groupName = id => store.state.groups.find(g => g.id === id)?.name || id;

export function avatar(user, size = "") {
  return html`<span class="${["avatar", size && `avatar-${size}`].filter(Boolean).join(" ")}" aria-hidden="true">${user.avatar ? html`<img src="${`/avatars/${encodeURIComponent(user.id)}?v=${store.avatarVersion}`}" alt="" decoding="async" loading="lazy">` : initial(user.display_name || user.username)}</span>`;
}
const person = (u, size = "") => html`<div class="person">${avatar(u, size)}<div><strong>${u.display_name || u.username}</strong><small class="muted">@${u.username}${u.owner ? " · Owner" : ""}</small></div></div>`;

export function accountsPage() {
  const { state } = store;
  const pending = state.users.filter(u => u.status === "pending");
  const rows = state.users.map(u => ({
    key: u.id,
    cells: [
      person(u),
      Status(statusLabel(u.status), statusTone[u.status] ?? "neutral"),
      html`<div class="badge-row">${state.memberships.filter(([uid]) => uid === u.id).map(([, gid]) => Badge(groupName(gid), "neutral"))}</div>`,
      Button({ label: u.status === "pending" ? "Review" : "Manage", action: "user-manage", id: u.id, kind: u.status === "pending" ? "primary" : "secondary", size: "sm" }),
    ],
  }));
  const groups = state.groups.map(g => {
    const members = state.memberships.filter(([, gid]) => gid === g.id).length;
    return Card({ tag: "article", className: "group-card", title: g.name, description: plural(members, "member"), actions: g.id !== "members" ? IconButton({ icon: "trash", label: `Delete ${g.name}`, action: "group-delete", id: g.id, size: "sm" }) : Badge("Everyone", "neutral") });
  });
  return html`${PageHeader({ title: "People & groups", description: "The people who make this universe yours.", actions: Button({ label: "Create account", action: "user-new", kind: "primary", icon: "plus" }), realm: "people" })}${pending.length ? Notice({ tone: "warn", icon: "people", body: `${plural(pending.length, "account request")} waiting for your review.` }) : ""}${Table({ columns: ["Person", "Status", "Groups", ""], rows })}<section class="page-section">${SectionHead({ title: "Groups", count: state.groups.length, actions: Button({ label: "New group", action: "group-new", icon: "plus", size: "sm" }) })}<div class="card-grid compact">${groups}</div></section>`;
}

export function userManage(id) {
  const { state } = store;
  const u = state.users.find(u => u.id === id);
  const labels = { approved: "Approve", suspended: "Suspend", rejected: "Reject" };
  const statusButtons = u.owner ? [] : Object.keys(labels).filter(s => s !== u.status).map(s => Button({ label: labels[s], action: "user-status", id, data: { status: s }, kind: s === "approved" ? "primary" : "danger", icon: s === "approved" ? "check" : "close", size: "sm" }));
  const membership = html`<div class="check-list">${state.groups.map(g => html`<label class="check"><input${attrs({ type: "checkbox", "data-membership": id, "data-group": g.id, checked: state.memberships.some(([uid, gid]) => uid === id && gid === g.id) })}><span>${g.name}</span></label>`)}</div>`;
  modal(u.display_name || u.username, html`<div class="person person-lg">${avatar(u, "lg")}<div><strong>@${u.username}</strong>${Status(statusLabel(u.status), statusTone[u.status] ?? "neutral")}</div></div>${u.bio ? html`<p class="bio">${u.bio}</p>` : ""}${ButtonGroup(...statusButtons, Button({ label: "Issue password reset link", action: "reset-issue", id, icon: "key", size: "sm" }))}${DialogSection("Group membership", membership)}${quotaSection(id)}`);
}

export function activityPage() {
  const users = Object.fromEntries(store.state.users.map(u => [u.id, u]));
  const rows = store.state.audit.map(a => {
    const who = users[a.actor];
    return {
      cells: [
        html`<span class="nowrap">${ago(a.created)}</span>`,
        who ? html`<div class="person person-sm">${avatar(who, "sm")}<span>${who.username}</span></div>` : html`<span class="muted">${a.actor || "System"}</span>`,
        html`<code class="action-name">${a.action}</code>`,
        html`<span class="detail">${a.detail}</span>`,
      ],
    };
  });
  return html`${PageHeader({ title: "Activity", description: "The recent story of this server.", realm: "activity" })}${rows.length ? Table({ columns: ["When", "Who", "Action", "Resource"], rows }) : EmptyState({ title: "Nothing has happened yet.", text: "Sign-ins, changes, and service events will appear here.", rune: "ᛞ" })}`;
}
