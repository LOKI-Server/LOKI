// Skald repositories: the list, and a page for each one laid out like a code host:
// Code, Commits, Issues & pull requests, and Settings for people who manage it.
import { ago, api, enc, modal, store } from "../lib/core.js";
import { accountUrl } from "./account.js";
import { ownerIcon } from "./people.js";
import { attrs, escape, html, plural, raw } from "../lib/html.js";
import { icon } from "../lib/icons.js";
import { Badge, Button, Checkbox, CopyBlock, CopyField, EmptyState, Field, Form, FormFooter, Help, IconButton, IconLink, LinkButton, List, Menu, MenuList, Notice, PageHeader, Pager, Row, Select, Submit, Tabs, TextArea, initial } from "../lib/ui.js";

const repoApi = id => `/api/repos/${enc(id)}`;
/** A repository page URL. Empty parameters are left out. */
export function repoUrl(id, params = {}) {
  const query = new URLSearchParams(Object.entries(params).filter(([, v]) => v !== undefined && v !== null && v !== ""));
  return `/repositories/${enc(id)}${query.size ? `?${query}` : ""}`;
}
const roles = { read: "Read", write: "Write", manage: "Manage" };
const short = oid => String(oid).slice(0, 7);
const avatarOf = name => html`<span class="avatar avatar-xs" aria-hidden="true">${initial(name)}</span>`;

// Markdown, enough for READMEs and discussions. The whole source is escaped first;
// only the tags below are added, and links are limited to http(s). That is why the
// result can be trusted as raw markup.
function inline(text) {
  return text.split(/(`[^`]+`)/).map(part => part.startsWith("`") && part.endsWith("`") && part.length > 1
    ? `<code>${part.slice(1, -1)}</code>`
    : part
      .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
      .replace(/(^|[\s(])[*_]([^*_\s][^*_]*)[*_](?=[\s).,!?:;]|$)/g, "$1<em>$2</em>")
      .replace(/\[([^\]]+)\]\((https?:\/\/[^\s)"']+)\)/g, '<a href="$2" target="_blank" rel="noopener noreferrer">$1</a>')).join("");
}
export function markdown(source) {
  let out = "", para = [], list = null, code = null;
  const endPara = () => { if (para.length) out += `<p>${inline(para.join(" "))}</p>`; para = []; };
  const endList = () => { if (list) out += `</${list}>`; list = null; };
  for (const line of escape(source).split(/\r?\n/)) {
    if (code) {
      if (/^\s*(```|~~~)/.test(line)) { out += `<pre><code>${code.join("\n")}</code></pre>`; code = null; }
      else code.push(line);
      continue;
    }
    if (/^\s*(```|~~~)/.test(line)) { endPara(); endList(); code = []; continue; }
    const head = /^(#{1,6})\s+(.*?)\s*#*$/.exec(line);
    if (head) { endPara(); endList(); out += `<h${head[1].length}>${inline(head[2])}</h${head[1].length}>`; continue; }
    if (/^\s*([-*_])(\s*\1){2,}\s*$/.test(line)) { endPara(); endList(); out += "<hr>"; continue; }
    const item = /^\s*(?:[-*+]|(\d+)[.)])\s+(.*)$/.exec(line);
    if (item) {
      endPara();
      const kind = item[1] ? "ol" : "ul";
      if (list !== kind) { endList(); out += `<${kind}>`; list = kind; }
      out += `<li>${inline(item[2])}</li>`;
      continue;
    }
    const quote = /^\s*&gt;\s?(.*)$/.exec(line);
    if (quote) { endPara(); endList(); out += `<blockquote>${inline(quote[1])}</blockquote>`; continue; }
    if (!line.trim()) { endPara(); endList(); continue; }
    endList();
    para.push(line.trim());
  }
  if (code) out += `<pre><code>${code.join("\n")}</code></pre>`;
  endPara();
  endList();
  return raw(`<div class="markdown">${out}</div>`);
}

// The repository list.
export async function vcsPage() {
  const { repositories, create, groups = [], transfers = [] } = await api("/api/repos");
  const names = Object.fromEntries(repositories.map(r => [r.id, `${r.owner?.name ?? ""} / ${r.name}`]));
  const canCreate = create || groups.length > 0;
  const actions = [
    store.session.user.owner ? Button({ label: "Manage", action: "vcs-manage", icon: "settings" }) : "",
    LinkButton({ label: "Skald tokens", href: accountUrl("tokens"), icon: "key" }),
    canCreate ? Button({ label: "New repository", action: "vcs-new", kind: "primary", icon: "plus" }) : "",
  ];
  const rows = repositories.map(repo => Row({
    key: repo.id, lead: repo.owner ? html`<span class="row-lead">${ownerIcon(repo.owner, "md")}</span>` : "", href: repoUrl(repo.id),
    title: html`<span class="owner-prefix">${repo.owner?.name} /</span> ${repo.name}`,
    badges: [repo.owner?.kind === "group" ? Badge("Group", "info") : "", Badge(roles[repo.role] || repo.role, "neutral")],
    meta: repo.upstream ? html`${icon("branch")}Forked from ${names[repo.upstream] ? html`<a href="${repoUrl(repo.upstream)}">${names[repo.upstream]}</a>` : "another repository"}` : "",
  }));
  return html`${PageHeader({ title: "Repositories", description: "Private version control for your projects, hosted by Skald.", actions, realm: "repos" })}${transfers.map(t => transferNotice(t, true))}${rows.length ? List(rows, { className: "list-boxed" }) : EmptyState({ title: "No repositories yet", text: canCreate ? "Create a repository, then clone it with skald and push your first commit." : "Repositories shared with you appear here.", action: canCreate ? Button({ label: "New repository", action: "vcs-new", kind: "primary", icon: "plus" }) : "", rune: "ᚨ" })}`;
}

/**
 * A waiting transfer: who it goes to, that code changes are paused, and the buttons
 * this person may use. `named` adds the repository's name, for the list page.
 */
function transferNotice(t, named = false) {
  // The recipient may not be able to open the repository until they accept it.
  const label = `${t.from.name} / ${t.repo_name}`;
  const what = !named ? "this repository" : t.can_decide && t.to.kind === "user" ? html`<strong>${label}</strong>` : html`<a href="${repoUrl(t.repo_id)}">${label}</a>`;
  const group = html`the group <strong>${t.to.name}</strong>`;
  const body = t.can_decide
    ? html`<strong>${t.requested_by}</strong> wants to give ${what} to ${t.to.kind === "group" ? group : "you"}. Code changes are paused until you accept or decline.`
    : t.to.kind === "group"
      ? html`Waiting for the server owner to accept ${what} into ${group}. Code changes are paused until then.`
      : html`Waiting for <strong>${t.to.name}</strong> to accept ${what}. Code changes are paused until then.`;
  const decide = (label, change, kind = "secondary") => Button({ label, action: "vcs-transfer-decide", id: t.repo_id, data: { change }, kind, size: "sm" });
  const buttons = [t.can_decide ? decide("Accept", "accept", "primary") : "", t.can_decide ? decide("Decline", "decline", "ghost") : "", t.can_cancel && !t.can_decide ? decide("Cancel transfer", "cancel", "ghost") : ""];
  return Notice({ tone: t.can_decide ? "warn" : "info", icon: "repo", body, action: html`<div class="notice-buttons">${buttons}</div>` });
}

/** The owners a new repository can have: yourself, and groups you may create in. */
function ownerChoice(create, groups) {
  const options = [...(create ? [["", `You (@${store.session.user.username})`]] : []), ...groups.map(g => [g.id, `Group: ${g.name}`])];
  if (options.length < 2) return options.length ? html`<input type="hidden" name="group" value="${options[0][0]}">` : "";
  return Select({ name: "group", label: "Owner", options, help: "Members of a group can push to its repositories. The server owner manages groups." });
}

export async function newRepo(upstream = "") {
  const { create, groups = [] } = await api("/api/repos");
  modal(upstream ? "Fork repository" : "New repository", Form({ name: "vcs-create", id: upstream, body: html`${ownerChoice(create, groups)}${Field({ name: "name", label: "Repository name", required: true, maxlength: 64, pattern: "[a-z0-9-]+", placeholder: "my-project", autocomplete: "off", help: "Lowercase letters, numbers, and dashes." })}${upstream ? html`<p>The fork copies the history and starts with its own access list.</p>` : Field({ name: "default_branch", label: "Default branch", value: "main", required: true, maxlength: 160 })}${FormFooter(Submit({ label: upstream ? "Create fork" : "Create repository" }))}` }));
}

// One repository.
export async function vcsRepo(id) {
  const params = new URLSearchParams(location.search);
  const view = {
    tab: params.get("tab") || "code",
    branch: params.get("branch") || "",
    path: params.get("path") || "",
    preview: params.get("preview") || "",
    revision: params.get("revision") || "",
    offset: params.get("offset") || "",
    source: params.has("source"),
  };
  const query = new URLSearchParams({ path: view.tab === "code" ? view.path : "" });
  for (const key of ["branch", "revision"]) if (view[key]) query.set(key, view[key]);
  if (view.offset && view.tab === "code") query.set("offset", view.offset);
  const before = params.get("before");
  const [repo, discussions] = await Promise.all([
    api(`${repoApi(id)}?${query}`),
    api(`${repoApi(id)}/items${before ? `?before=${enc(before)}` : ""}`),
  ]);
  // Links keep the branch or revision being looked at.
  view.at = { branch: view.branch || undefined, revision: view.revision || undefined };
  const manager = repo.role === "manage" || repo.can_delete;
  if (view.tab === "settings" && !manager) view.tab = "code";
  const open = discussions.items.filter(i => i.status === "open").length;
  const body = view.tab === "commits" ? commitsTab(id, repo, view)
    : view.tab === "issues" ? issuesTab(id, discussions, before)
    : view.tab === "settings" ? await settingsTab(id, repo)
    : await codeTab(id, repo, view);
  return html`${repoHeader(repo)}${repo.transfer ? transferNotice(repo.transfer) : ""}${Tabs([
    { label: "Code", href: repoUrl(id, view.at), active: view.tab === "code", icon: "code" },
    { label: "Commits", href: repoUrl(id, { ...view.at, tab: "commits" }), active: view.tab === "commits", icon: "history" },
    { label: "Issues & pull requests", href: repoUrl(id, { tab: "issues" }), active: view.tab === "issues", count: open ? `${open}${discussions.next ? "+" : ""}` : 0, icon: "issue" },
    manager && { label: "Settings", href: repoUrl(id, { tab: "settings" }), active: view.tab === "settings", icon: "settings" },
  ], "Repository")}${body}`;
}

const cloneCommand = repo => `skald clone ${repo.remote_url} ${repo.name}`;
const tokenHelp = html`<p class="help">Set <code>SKALD_TOKEN</code> to one of <a href="${accountUrl("tokens")}">your Skald tokens</a> first.</p>`;

function repoTitle(id, name, extra = "", owner = null) {
  const by = owner ? html`<span class="repo-owner">${ownerIcon(owner, "xs")}${owner.name}</span><span class="slash">/</span>` : "";
  return html`<div class="repo-title">${icon("repo")}<a href="/repositories">Repositories</a><span class="slash">/</span>${by}<a class="repo-name" href="${repoUrl(id)}">${name}</a>${extra}</div>`;
}

function repoHeader(repo) {
  const panel = html`<h4 class="menu-title">Clone with Skald</h4>${CopyField(cloneCommand(repo), "Copy clone command")}${tokenHelp}`;
  return html`<header class="repo-header"><div>${repoTitle(repo.id, repo.name, Badge("Private", "neutral"), repo.owner)}${repo.upstream ? html`<p class="repo-fork">${icon("branch")}Forked from <a href="${repoUrl(repo.upstream.id)}">${repo.upstream.name}</a></p>` : ""}</div><div class="repo-actions">${repo.can_fork ? Button({ label: "Fork", action: "vcs-fork", id: repo.id, icon: "branch", size: "sm" }) : ""}${Menu({ trigger: html`${icon("code")}<span>Code</span>`, body: panel, kind: "primary", size: "sm", align: "right", label: "Clone this repository" })}</div></header>`;
}

/** The branch and tag picker. Choosing one keeps the current tab and folder. */
function refPicker(id, repo, view, extra = {}) {
  const branches = repo.branches.map(b => ({ label: b, href: repoUrl(id, { ...extra, branch: b }), current: b === repo.branch && !view.revision, badge: b === repo.default_branch ? Badge("default", "neutral") : "" }));
  const tags = Object.entries(repo.tags).map(([name, oid]) => ({ label: name, href: repoUrl(id, { ...extra, revision: oid }), current: view.revision === oid }));
  return Menu({ trigger: html`${icon(view.revision ? "commit" : "branch")}<span class="ref-name">${view.revision ? short(repo.revision) : repo.branch}</span>`, size: "sm", label: "Switch branch or tag", body: html`${branches.length ? MenuList("Branches", branches) : Help("No branches yet.")}${tags.length ? MenuList("Tags", tags) : ""}` });
}

function quickSetup(repo) {
  const steps = `${cloneCommand(repo)}\ncd ${repo.name}\nskald add .\nskald commit --author "Your Name" -m "First commit"\nskald push`;
  return html`<div class="box"><div class="box-head"><strong>Quick setup</strong></div><div class="box-body"><p>This repository is empty. Clone it, add your files, and push the first commit to <code>${repo.default_branch}</code>.</p>${CopyBlock(steps, "Copy commands")}${tokenHelp}</div></div>`;
}

function crumbs(id, repo, view) {
  let prefix = "";
  const links = view.path.split("/").filter(Boolean).map(part => {
    prefix += (prefix ? "/" : "") + part;
    return html`<span class="slash">/</span><a href="${repoUrl(id, { ...view.at, path: prefix })}">${part}</a>`;
  });
  const file = view.preview ? html`<span class="slash">/</span><strong>${view.preview.split("/").pop()}</strong>` : "";
  return html`<nav class="repo-path" aria-label="Path"><a href="${repoUrl(id, view.at)}">${repo.name}</a>${links}${file}</nav>`;
}

function latestCommit(id, repo, view) {
  const c = repo.commits[0];
  if (!c) return "";
  const count = repo.commits.length >= 30 ? "30+" : repo.commits.length;
  return html`<div class="box-head commit-bar">${avatarOf(c.author)}<strong>${c.author}</strong><a class="commit-title" href="${repoUrl(id, { revision: c.oid })}" title="${c.title}">${c.title || "(no message)"}</a><span class="commit-meta"><code>${short(c.oid)}</code>${ago(c.time)}<a href="${repoUrl(id, { ...view.at, tab: "commits" })}">${icon("history")}${count} commit${count === 1 ? "" : "s"}</a></span></div>`;
}

const fileIcon = name => /\.(png|jpe?g|gif|webp|svg|bmp|ico)$/i.test(name) ? "image" : /\.(mp4|webm|mov|mkv)$/i.test(name) ? "video" : /\.(mp3|wav|ogg|flac)$/i.test(name) ? "audio" : /\.(md|txt|rst|json|toml|ya?ml|[ch]pp|[ch]|rs|js|ts|py|cs|go|java|lua|gd|html|css|sh|ps1)$/i.test(name) ? "text" : "file";
const isMarkdown = name => /\.(md|markdown)$/i.test(name);

async function fileText(id, repo, view, path) {
  const query = new URLSearchParams({ branch: repo.branch, path });
  if (view.revision) query.set("revision", view.revision);
  const url = `${repoApi(id)}/file?${query}`;
  try { return { url, text: (await api(url)).text ?? null }; }
  catch (error) { return { url, text: null, error: error.message }; }
}

async function codeTab(id, repo, view) {
  if (!repo.branches.length && !view.revision) return html`<div class="repo-layout"><div class="repo-main">${quickSetup(repo)}</div>${about(id, repo)}</div>`;
  const banner = view.revision ? Notice({ tone: "info", icon: "commit", body: html`Viewing files at commit <code>${short(repo.revision)}</code>.`, action: html`<a href="${repoUrl(id, { branch: view.branch || undefined })}">Back to ${repo.branch}</a>` }) : "";
  const toolbar = html`<div class="repo-toolbar">${refPicker(id, repo, view, { path: view.path })}${crumbs(id, repo, view)}</div>`;
  let main;
  if (view.preview) main = await fileView(id, repo, view);
  else {
    const entries = [...repo.tree].sort((a, b) => (a.kind === "tree" ? 0 : 1) - (b.kind === "tree" ? 0 : 1) || a.name.localeCompare(b.name));
    const parent = view.path ? html`<a class="file-row" href="${repoUrl(id, { ...view.at, path: view.path.split("/").slice(0, -1).join("/") })}">${icon("up")}<span>..</span></a>` : "";
    const rows = entries.map(e => {
      const full = view.path ? `${view.path}/${e.name}` : e.name;
      const href = e.kind === "tree" ? repoUrl(id, { ...view.at, path: full }) : repoUrl(id, { ...view.at, path: view.path, preview: full });
      return html`<a class="${e.kind === "tree" ? "file-row dir" : "file-row"}" href="${href}" data-key="${full}">${icon(e.kind === "tree" ? "folder" : fileIcon(e.name))}<span>${e.name}</span></a>`;
    });
    main = html`<div class="box">${latestCommit(id, repo, view)}<div class="file-rows">${parent}${rows.length ? rows : html`<p class="list-empty">No files here.</p>`}</div></div>${Pager({ previous: view.offset ? repoUrl(id, { ...view.at, path: view.path }) : "", previousLabel: "First files", next: repo.next_offset ? repoUrl(id, { ...view.at, path: view.path, offset: repo.next_offset }) : "", nextLabel: "More files" })}`;
    const readme = repo.tree.find(e => e.kind !== "tree" && /^readme(\.(md|markdown|txt))?$/i.test(e.name));
    if (readme) {
      const { text } = await fileText(id, repo, view, view.path ? `${view.path}/${readme.name}` : readme.name);
      if (text !== null) main = html`${main}<div class="box readme"><div class="box-head">${icon("text")}<strong>${readme.name}</strong></div><div class="box-body">${isMarkdown(readme.name) ? markdown(text) : html`<pre class="plain">${text}</pre>`}</div></div>`;
    }
  }
  return html`${banner}<div class="repo-layout"><div class="repo-main">${toolbar}${main}</div>${about(id, repo)}</div>`;
}

async function fileView(id, repo, view) {
  const name = view.preview.split("/").pop();
  const { url, text, error } = await fileText(id, repo, view, view.preview);
  const lines = text === null ? [] : text.replace(/\n$/, "").split("\n");
  const rendered = text !== null && isMarkdown(name) && !view.source;
  const here = { ...view.at, path: view.path, preview: view.preview };
  const toggle = text !== null && isMarkdown(name)
    ? html`<div class="segmented">${LinkButton({ label: "Preview", href: repoUrl(id, here), size: "sm", kind: rendered ? "secondary" : "ghost" })}${LinkButton({ label: "Source", href: repoUrl(id, { ...here, source: 1 }), size: "sm", kind: rendered ? "ghost" : "secondary" })}</div>` : "";
  const content = text === null
    ? EmptyState({ compact: true, rune: "ᛈ", text: error || "This file can't be shown as text.", action: LinkButton({ label: "Download", href: `${url}&raw=true`, download: true, icon: "download", size: "sm" }) })
    : rendered ? html`<div class="box-body">${markdown(text)}</div>`
    : html`<div class="code-view"><table><tbody>${lines.map((line, i) => html`<tr><td class="ln" data-n="${i + 1}"></td><td class="lc">${line || " "}</td></tr>`)}</tbody></table></div>`;
  return html`<div class="box file-view"><div class="box-head">${toggle}<span class="muted">${text === null ? "" : plural(lines.length, "line")}</span><div class="box-tools">${text === null ? "" : IconButton({ icon: "copy", label: "Copy file contents", action: "copy", data: { copy: text }, size: "sm" })}${IconLink({ icon: "download", label: `Download ${name}`, href: `${url}&raw=true`, download: true, size: "sm" })}</div></div>${content}</div>`;
}

function about(id, repo) {
  const tags = Object.entries(repo.tags);
  return html`<aside class="repo-aside"><h3>About</h3><p>${repo.upstream ? html`A fork of <a href="${repoUrl(repo.upstream.id)}">${repo.upstream.name}</a>.` : "Private Skald repository."}</p><ul class="meta-list"><li>${icon("branch")}<span>${plural(repo.branches.length, "branch", "branches")}</span></li><li>${icon("tag")}<span>${plural(tags.length, "tag")}</span></li><li>${icon("shield")}${repo.protect_default ? html`<span><code>${repo.default_branch}</code> needs review</span>` : html`<span>Default branch <code>${repo.default_branch}</code></span>`}</li><li>${icon("user")}<span>Your access: ${roles[repo.role] || repo.role}</span></li></ul>${tags.length ? html`<h3>Tags</h3><div class="badge-row">${tags.slice(0, 12).map(([name, oid]) => html`<a class="badge badge-neutral" href="${repoUrl(id, { revision: oid })}">${icon("tag")}${name}</a>`)}</div>` : ""}</aside>`;
}

function commitsTab(id, repo, view) {
  if (!repo.commits.length) return EmptyState({ compact: true, rune: "ᚨ", text: "No commits yet." });
  const days = new Map();
  for (const c of repo.commits) {
    const day = new Date(c.time * 1000).toLocaleDateString(undefined, { year: "numeric", month: "long", day: "numeric" });
    if (!days.has(day)) days.set(day, []);
    days.get(day).push(c);
  }
  const groups = [...days].map(([day, commits]) => html`<div class="commit-group"><h3>${icon("commit")}Commits on ${day}</h3>${List(commits.map(c => Row({
    key: c.oid, title: c.title || "(no message)", href: repoUrl(id, { revision: c.oid }),
    meta: html`${avatarOf(c.author)}${c.author} committed ${ago(c.time)}`,
    actions: [html`<code class="sha">${short(c.oid)}</code>`, IconButton({ icon: "copy", label: "Copy full commit ID", action: "copy", data: { copy: c.oid }, size: "sm" }), IconLink({ icon: "code", label: "Browse files at this commit", href: repoUrl(id, { revision: c.oid }), size: "sm" })],
  })), { className: "list-boxed" })}</div>`);
  return html`<div class="repo-toolbar">${refPicker(id, repo, view, { tab: "commits" })}</div>${groups}${repo.commits.length >= 30 ? html`<p class="help center">Showing the 30 most recent commits. Use <code>skald log</code> for the full history.</p>` : ""}`;
}

const itemState = item => item.status === "merged" ? ["merge", "merged"] : item.kind === "pull" ? ["pull", item.status] : ["issue", item.status];
const stateIcon = item => { const [name, state] = itemState(item); return html`<span class="${`state-icon state-${state}`}" title="${state}">${icon(name)}</span>`; };

function issuesTab(id, discussions, before) {
  const rows = discussions.items.map(item => Row({ key: item.id, lead: stateIcon(item), title: item.title, href: `/repositories/${enc(id)}/items/${enc(item.id)}`, meta: `${item.kind === "pull" ? "Pull request" : "Issue"} · ${item.status}` }));
  return html`<div class="repo-toolbar end">${Button({ label: "New issue", action: "vcs-new-item", id, data: { kind: "issue" }, icon: "issue", size: "sm" })}${Button({ label: "New pull request", action: "vcs-new-item", id, data: { kind: "pull" }, kind: "primary", icon: "pull", size: "sm" })}</div>${rows.length ? List(rows, { className: "list-boxed" }) : EmptyState({ compact: true, rune: "ᚨ", title: "No issues or pull requests", text: "Track work with an issue, or propose changes from a branch with a pull request." })}${Pager({ previous: before ? repoUrl(id, { tab: "issues" }) : "", previousLabel: "Newest", next: discussions.next ? repoUrl(id, { tab: "issues", before: discussions.next }) : "", nextLabel: "Older" })}`;
}

const block = (title, text, body, danger = false) => html`<section class="${danger ? "setting-block danger" : "setting-block"}"><h2>${title}</h2>${text ? html`<p>${text}</p>` : ""}${body}</section>`;

async function settingsTab(id, repo) {
  const branches = [...new Set([repo.default_branch, ...repo.branches])];
  const blocks = [block("General", "", Form({ name: "vcs-settings", id, body: html`${Select({ name: "default_branch", label: "Default branch", options: branches, value: repo.default_branch, help: "Shown first, and what clones check out." })}${Checkbox({ name: "protect_default", label: "Require an approved pull request to update the default branch", checked: repo.protect_default })}${FormFooter(Submit({ label: "Save" }))}` }))];
  const permissions = repo.role === "manage" ? await api(`${repoApi(id)}/permissions`) : null;
  if (permissions) {
    const { users, groups, grants } = permissions;
    const options = [...users.map(([key, name]) => [`user:${key}`, name, "user"]), ...groups.map(([key, name]) => [`group:${key}`, name, "people"])];
    const labels = new Map(options.map(([key, name, kind]) => [key, [name, kind]]));
    const rows = grants.map(([kind, subject, role]) => {
      const [name, iconName] = labels.get(`${kind}:${subject}`) || [subject, kind === "group" ? "people" : "user"];
      return Row({ key: `${kind}:${subject}`, icon: iconName, title: name, badges: Badge(roles[role] || role, "neutral"), meta: kind === "group" ? "Group" : "Person", actions: IconButton({ icon: "trash", label: `Remove ${name}`, action: "vcs-remove-access", id, data: { kind, subject }, size: "sm" }) });
    });
    const add = Form({ name: "vcs-grant", id, className: "inline-form", body: html`${Select({ name: "subject", label: "Person or group", hiddenLabel: true, options: options.map(([key, name, kind]) => [key, kind === "people" ? `Group: ${name}` : name]) })}${Select({ name: "role", label: "Role", hiddenLabel: true, options: [["read", "Read"], ["write", "Write"], ["manage", "Manage"]] })}${Submit({ label: "Add", icon: "plus" })}` });
    blocks.push(block("Access", "Read can clone and join discussions. Write can push. Manage can change settings and access.", html`${List(rows, { className: "list-boxed", empty: "Only you and the server owner can reach this repository." })}${add}`));
    blocks.push(block("Maintenance", "", List([
      Row({ title: "Verify repository", meta: "Check every stored object against its hash.", actions: Button({ label: "Verify", action: "vcs-verify", id, icon: "check", size: "sm" }) }),
      Row({ title: "Collect old objects", meta: "Free space held by data that no branch, tag, or open pull request needs.", actions: Button({ label: "Collect", action: "vcs-gc", id, icon: "archive", size: "sm" }) }),
    ], { className: "list-boxed" })));
  }
  if (repo.can_transfer && permissions) blocks.push(block("Transfer ownership", "Give this repository to a person or a group. Its history, discussions and access list go with it.", repo.transfer
    ? Notice({ tone: "info", body: "A transfer is already waiting. Cancel it from the banner above to choose someone else." })
    : transferForm(id, repo, permissions)));
  if (repo.can_delete) blocks.push(block("Danger zone", "", List(Row({ title: "Delete this repository", meta: "Removes every revision, file, and discussion. This can't be undone.", actions: Button({ label: "Delete repository", action: "vcs-delete", id, data: { name: repo.name }, kind: "danger", icon: "trash", size: "sm" }) }), { className: "list-boxed danger-zone" }), true));
  return html`<div class="settings-stack">${blocks}</div>`;
}

function transferForm(id, repo, { users, groups }) {
  const mine = repo.owner.kind === "user" ? `user:${repo.owner.id}` : `group:${repo.owner.id}`;
  const options = [...groups.map(([key, name]) => [`group:${key}`, `Group: ${name}`]), ...users.map(([key, name]) => [`user:${key}`, `@${name}`])].filter(([key]) => key !== mine);
  if (!options.length) return Help("There's nobody else to transfer it to yet.");
  return html`${Form({ name: "vcs-transfer", id, className: "inline-form transfer-form", body: html`${Select({ name: "target", label: "New owner", hiddenLabel: true, options })}${Submit({ label: "Transfer…", kind: "secondary", icon: "repo" })}` })}${Help("Anyone other than you, and groups unless you're the server owner, must accept first. Pushes and merges pause until they decide.")}`;
}

export async function newItem(id, kind) {
  let source = "";
  if (kind === "pull") {
    const repo = await api(repoApi(id));
    const branches = repo.branches.filter(b => b !== repo.default_branch);
    const into = html`It will be merged into <code>${repo.default_branch}</code>.`;
    source = branches.length
      ? Select({ name: "branch", label: "Branch with your changes", options: branches, help: into })
      : Field({ name: "branch", label: "Branch with your changes", required: true, maxlength: 160, help: html`Push a branch first. ${into}` });
  }
  modal(kind === "pull" ? "New pull request" : "New issue", Form({ name: "vcs-item", id, body: html`<input type="hidden" name="kind" value="${kind}">${Field({ name: "title", label: "Title", required: true, maxlength: 160 })}${source}${TextArea({ name: "body", label: "Description", rows: 6, maxlength: 12000, placeholder: "Markdown is supported." })}${FormFooter(Submit({ label: kind === "pull" ? "Open pull request" : "Open issue" }))}` }));
}

function diffView(diff) {
  return html`<pre class="diff">${diff.split("\n").map(line => {
    const kind = line.startsWith("+++") || line.startsWith("---") || /^(diff|index) /.test(line) ? "meta" : line.startsWith("+") ? "add" : line.startsWith("-") ? "del" : line.startsWith("@@") ? "hunk" : "";
    return html`<span${attrs({ class: kind || null })}>${line || " "}</span>`;
  })}</pre>`;
}

export async function vcsItem(id, itemId) {
  const [{ item, role, comments, diff }, list] = await Promise.all([api(`${repoApi(id)}/items/${enc(itemId)}`), api("/api/repos").catch(() => ({ repositories: [] }))]);
  const listed = list.repositories.find(r => r.id === id);
  const repoName = listed?.name || "Repository";
  const mine = item.author_id === store.session.user.id;
  const change = (label, action, kind = "secondary", iconName = "") => Button({ label, action: "vcs-item-action", id, data: { item: item.id, change: action, head: item.head || "", base: item.base || "" }, kind, icon: iconName, block: true });
  const buttons = [];
  if (item.status === "open" && item.kind === "pull") {
    if (role === "manage") buttons.push(item.approved_head ? change("Merge pull request", "merge", "primary", "merge") : change("Approve changes", "approve", "primary", "check"));
    if (mine) buttons.push(change("Refresh from branch", "refresh", "secondary", "refresh"));
  }
  if (item.status !== "merged" && (mine || role === "manage")) buttons.push(change(item.status === "open" ? "Close" : "Reopen", item.status === "open" ? "close" : "reopen", "secondary", item.status === "open" ? "close" : "refresh"));
  const [iconName, state] = itemState(item);
  const noun = item.kind === "pull" ? "pull request" : "issue";
  const branches = item.kind === "pull" && item.source_branch ? html` · wants to merge <code>${item.source_branch}</code> into <code>${item.base_ref || "the default branch"}</code>` : "";
  const pill = html`<span class="${`state-pill state-${state}`}">${icon(iconName)}${state[0].toUpperCase() + state.slice(1)}</span>`;
  const comment = (who, when, body, first = false) => html`<article class="comment"><span class="avatar" aria-hidden="true">${initial(who)}</span><div class="box"><div class="box-head"><strong>${who}</strong><span class="muted">${first ? "opened this" : "commented"} ${ago(when)}</span></div><div class="box-body">${body.trim() ? markdown(body) : html`<p class="muted">No description provided.</p>`}</div></div></article>`;
  const reply = Form({ name: "vcs-comment", id, data: { item: item.id }, className: "comment", body: html`<span class="avatar" aria-hidden="true">${initial(store.session.user.username)}</span><div class="box"><div class="box-body">${TextArea({ name: "body", label: "Comment", hiddenLabel: true, rows: 4, maxlength: 6000, required: true, placeholder: "Leave a comment. Markdown is supported." })}${FormFooter(Submit({ label: "Comment" }), { divider: false })}</div></div>` });
  return html`<header class="repo-header">${repoTitle(id, repoName, html`<span class="slash">/</span><a href="${repoUrl(id, { tab: "issues" })}">Issues &amp; pull requests</a>`, listed?.owner)}</header><div class="item-head"><h1>${item.title}</h1><p>${pill}<span><strong>${item.username}</strong> opened this ${noun} ${ago(item.created)}${branches}</span></p></div><div class="repo-layout"><div class="repo-main timeline">${comment(item.username, item.created, item.body, true)}${diff === null || diff === undefined ? "" : html`<div class="box"><div class="box-head">${icon("code")}<strong>Changes</strong>${item.approved_head ? Badge("Approved") : ""}</div>${diffView(diff)}</div>`}${comments.map(c => comment(c.username, c.created, c.body))}${reply}</div><aside class="repo-aside"><h3>Status</h3><p>${pill}${item.approved_head ? Badge("Approved") : ""}</p>${buttons.length ? html`<div class="stack-buttons">${buttons}</div>` : ""}</aside></div>`;
}
