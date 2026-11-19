import { api, date, enc, esc, modal, store } from "../lib/core.js";
import { button, check, empty, field, heading, linkButton, table, tag } from "../lib/ui.js";

const repoUrl = id => `/git/${enc(id)}`;
const repoApi = id => `/api/git/${enc(id)}`;
const actions = content => `<div class="actions">${content}</div>`;

export async function gitPage() {
  const { repositories, create } = await api("/api/git");
  const controls = (create ? button("New repository", "git-new", "", "", "primary", "plus") : "")
    + (store.session.user.owner ? button("Manage repository creation", "git-creation") : "");
  const rows = repositories.map(repo => ({ cells: [`<a href="${repoUrl(repo.id)}">${esc(repo.name)}</a>`, tag(repo.role, "dim")] }));
  return heading("Repositories", "Private Git repositories, issues, and pull requests.", controls, "git")
    + (rows.length ? table(["Repository", "Access"], rows) : empty("No repositories yet", "Repositories you can access appear here."));
}

export function newRepo(upstream) {
  modal(upstream ? "Fork repository" : "New repository", `<form data-form="git-create" data-id="${esc(upstream)}">
    ${field("name", "Repository name", "", "text", "required maxlength=64 pattern=[a-z0-9-]+")}
    ${upstream ? '<p class="help">Your private copy has its own permissions and retains the copied history.</p>' : field("default_branch", "Default branch", "main", "text", "required maxlength=200")}
    ${actions(`<button>${upstream ? "Create fork" : "Create repository"}</button>`)}</form>`);
}

export async function gitRepo(id) {
  const params = new URLSearchParams(location.search);
  const requested = params.get("branch");
  const path = params.get("path") || "";
  const preview = params.get("preview");
  const query = `path=${enc(path)}${requested ? `&branch=${enc(requested)}` : ""}`;
  const before = params.get("before");
  const [repo, discussions] = await Promise.all([api(`${repoApi(id)}?${query}`), api(`${repoApi(id)}/items${before ? `?before=${enc(before)}` : ""}`)]);
  const branch = repo.branch;
  const page = (key, value) => `${repoUrl(id)}?branch=${enc(branch)}&amp;${key}=${enc(value)}`;
  const controls = [linkButton("Repositories", "/git"), linkButton("Git credentials", "/account/git-credentials")];
  if (repo.can_fork && repo.branches.includes(repo.default_branch)) controls.push(button("Fork", "git-fork", id));
  if (repo.role === "manage") controls.push(button("Settings", "git-settings", id), button("Manage access", "git-access", id));
  if (repo.can_delete) controls.push(button("Delete repository", "git-delete", id, `data-name="${esc(repo.name)}"`, "danger"));
  const header = heading(repo.name, repo.upstream ? `Fork of ${repo.upstream.name}` : "Private repository", controls.join(""), "folder");
  const command = `git clone ${repo.clone_url}`;
  const clone = `<section class="card"><h2>Clone</h2><pre class="mono">${esc(command)}</pre>${button("Copy clone command", "git-copy", "", `data-copy="${esc(command)}"`)}${repo.branches.includes(repo.default_branch) ? "" : `<p>Push the first commit to <code>${esc(repo.default_branch)}</code>.</p>`}</section>`;
  const branches = repo.branches.map(ref => `<a class="button subtle" href="${repoUrl(id)}?branch=${enc(ref)}"${ref === branch ? ' aria-current="page"' : ""}>${esc(ref)}</a>`).join("");
  const tree = repo.tree.map(entry => {
    const full = path ? `${path}/${entry.name}` : entry.name;
    const link = entry.kind === "tree" ? page("path", full) : `${page("preview", full)}&amp;path=${enc(path)}`;
    return { cells: [entry.kind === "commit" ? esc(entry.name) : `<a href="${link}">${esc(entry.name)}</a>`, esc(entry.kind === "tree" ? "Folder" : entry.kind === "commit" ? "Submodule" : "File")] };
  });
  let contents = "";
  if (preview) {
    const url = `${repoApi(id)}/file?branch=${enc(branch)}&path=${enc(preview)}`;
    let text;
    try { ({ text } = await api(url)); } catch (error) { text = error.message; }
    contents = `<section class="card"><h2>${esc(preview)}</h2><a download href="${esc(url)}&amp;raw=true">Download file</a><pre class="log">${esc(text ?? "Binary file")}</pre></section>`;
  }
  const commits = repo.commits.map(commit => ({ cells: [esc(commit.title), esc(commit.author), `<code>${esc(commit.oid.slice(0, 12))}</code>`, esc(date(commit.time))] }));
  const items = discussions.items.map(item => ({ cells: [`<a href="${repoUrl(id)}/items/${enc(item.id)}">${esc(item.title)}</a>`, item.kind === "pull" ? "Pull request" : "Issue", tag(item.status, "dim")] }));
  const pagination = (before ? linkButton("Newest discussions", `${repoUrl(id)}?${query}`) : "")
    + (discussions.next ? linkButton("Older discussions", `${repoUrl(id)}?${query}&before=${enc(discussions.next)}`) : "");
  return header + clone + `<section class="card"><h2>Files</h2>${actions(branches)}${path ? `<a href="${page("path", path.split("/").slice(0, -1).join("/"))}">Parent folder</a>` : ""}${tree.length ? table(["Name", "Type"], tree) : '<p class="help">No files on this branch.</p>'}</section>`
    + contents + (commits.length ? `<section class="card"><h2>Recent commits</h2>${table(["Commit", "Author", "ID", "Date"], commits)}</section>` : "")
    + `<section class="card"><h2>Issues and pull requests</h2>${actions(button("New issue", "git-new-item", id, 'data-kind="issue"') + button("New pull request", "git-new-item", id, 'data-kind="pull"'))}${items.length ? table(["Title", "Type", "Status"], items) : '<p class="help">No issues or pull requests here.</p>'}${actions(pagination)}</section>`;
}

export async function gitAccess(id) {
  const { users, groups, grants } = await api(`${repoApi(id)}/permissions`);
  const options = [...users.map(([key, name]) => [`user:${key}`, `User: ${name}`]), ...groups.map(([key, name]) => [`group:${key}`, `Group: ${name}`])];
  const labels = new Map(options);
  const rows = grants.map(([kind, subject, role]) => ({ cells: [esc(labels.get(`${kind}:${subject}`) || subject), esc(role), button("Remove", "git-remove-access", id, `data-kind="${esc(kind)}" data-subject="${esc(subject)}"`, "danger")] }));
  modal("Repository access", `${rows.length ? table(["Account or group", "Role", ""], rows) : ""}<form data-form="git-grant" data-id="${esc(id)}"><label for="git-subject">Account or group</label><select id="git-subject" name="subject">${options.map(([key, name]) => `<option value="${esc(key)}">${esc(name)}</option>`).join("")}</select><label for="git-role">Role</label><select id="git-role" name="role"><option value="read">Read</option><option value="write">Write</option><option value="manage">Manage</option></select>${actions("<button>Save access</button>")}</form>`);
}

export async function gitSettings(id) {
  const repo = await api(repoApi(id));
  const branches = [...new Set([repo.default_branch, ...repo.branches])];
  modal("Repository settings", `<form data-form="git-settings" data-id="${esc(id)}">
    <label for="default-branch">Default branch</label><select id="default-branch" name="default_branch">${branches.map(branch => `<option ${branch === repo.default_branch ? "selected" : ""}>${esc(branch)}</option>`).join("")}</select>
    ${check("protect_default", "Require pull request review for the default branch", repo.protect_default)}
    ${actions("<button>Save settings</button>")}</form>`);
}

export async function newItem(id, kind) {
  const { repositories } = kind === "pull" ? await api("/api/git") : { repositories: [] };
  const source = `<label for="git-source">Source repository</label><select id="git-source" name="source"><option value="">This repository</option>${repositories.filter(repo => repo.upstream === id).map(repo => `<option value="${esc(repo.id)}">${esc(repo.name)}</option>`).join("")}</select>${field("branch", "Source branch", "", "text", "required maxlength=200")}<p class="help">Submitting from a fork shares the selected commit and its history with the target repository.</p>`;
  modal(kind === "pull" ? "New pull request" : "New issue", `<form data-form="git-item" data-id="${esc(id)}"><input type="hidden" name="kind" value="${esc(kind)}">${field("title", "Title", "", "text", "required maxlength=160")}<label for="git-body">Description</label><textarea id="git-body" name="body" maxlength="12000"></textarea>${kind === "pull" ? source : ""}${actions("<button>Submit</button>")}</form>`);
}

export async function gitItem(id, itemId) {
  const { item, role, comments, diff } = await api(`${repoApi(id)}/items/${enc(itemId)}`);
  const mine = item.author_id === store.session.user.id;
  const formButton = (label, action) => button(label, "git-item-action", id, `data-item="${esc(item.id)}" data-change="${action}" data-head="${esc(item.head)}" data-base="${esc(item.base)}"`);
  const buttons = [];
  if (item.status !== "merged" && (mine || role === "manage")) buttons.push(formButton(item.status === "open" ? "Close" : "Reopen", item.status === "open" ? "close" : "reopen"));
  if (item.status === "open" && item.kind === "pull") {
    if (mine) buttons.push(formButton("Refresh submission", "refresh"));
    if (role === "manage") {
      buttons.push(formButton("Approve submitted changes", "approve"));
      if (item.approved_head) buttons.push(formButton("Merge", "merge"));
    }
  }
  return heading(item.title, `${item.kind === "pull" ? "Pull request" : "Issue"} · ${item.status} · @${item.username}`, linkButton("Repository", repoUrl(id)), "folder")
    + `<section class="card"><p>${esc(item.body)}</p>${actions(buttons.join(""))}${item.approved_head ? tag("Approved", "dim") : ""}</section>`
    + (diff === null ? "" : `<section class="card"><h2>Changes</h2><pre class="log">${esc(diff)}</pre></section>`)
    + `<section class="card"><h2>Discussion</h2>${comments.map(comment => `<article><strong>@${esc(comment.username)}</strong><small>${esc(date(comment.created))}</small><p>${esc(comment.body)}</p></article>`).join("")}<form data-form="git-comment" data-id="${esc(id)}" data-item="${esc(item.id)}"><label for="git-comment">Comment</label><textarea id="git-comment" name="body" maxlength="6000" required></textarea>${actions("<button>Post comment</button>")}</form></section>`;
}
