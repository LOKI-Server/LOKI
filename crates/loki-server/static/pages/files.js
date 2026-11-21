// Browsing and previewing a file share. Folder, page, and sort live in the URL,
// so Back, reload, and shared links all land in the same place.
import { api, ago, enc, fmt, modal, resource } from "../lib/core.js";
import { attrs, html, plural } from "../lib/html.js";
import { icon } from "../lib/icons.js";
import { Breadcrumbs, EmptyState, FormFooter, IconLink, LinkButton, PageHeader, Pager, Table, Toolbar } from "../lib/ui.js";

const types = {
  image: ["png", "jpg", "jpeg", "webp", "gif", "avif"],
  audio: ["mp3", "ogg", "wav", "flac"],
  video: ["mp4", "webm"],
  text: ["pdf", "txt", "md", "rs", "json", "toml", "yaml", "yml", "csv", "log"],
};
const kindOf = name => {
  const ext = name.split(".").pop().toLowerCase();
  return Object.keys(types).find(kind => types[kind].includes(ext)) || "file";
};

export const browseUrl = (id, path = "", page = 0, sort = "name") => {
  const query = new URLSearchParams();
  if (path) query.set("path", path);
  if (page) query.set("page", page);
  if (sort !== "name") query.set("sort", sort);
  return `/shares/${enc(id)}${query.size ? `?${query}` : ""}`;
};
const fileUrl = (id, path) => `/api/shares/${enc(id)}/file?path=${enc(path)}`;

export async function browse(id, { path = "", page = 0, sort = "name" } = {}) {
  const r = resource(id);
  const listing = await api(`/api/shares/${enc(id)}?path=${enc(path)}&page=${page}&sort=${enc(sort)}`);
  let prefix = "";
  const parts = path.split("/").filter(Boolean);
  const crumbs = [{ label: r?.name || "Files", href: parts.length ? browseUrl(id, "", 0, sort) : "", icon: "folder" }, ...parts.map((part, i) => {
    prefix += (prefix ? "/" : "") + part;
    return { label: part, href: i === parts.length - 1 ? "" : browseUrl(id, prefix, 0, sort) };
  })];
  const rows = listing.entries.map(e => {
    const full = path ? `${path}/${e.name}` : e.name;
    const name = e.directory
      ? html`<a class="file-name" href="${browseUrl(id, full, 0, sort)}"><span class="file-icon folder" aria-hidden="true">${icon("folder")}</span>${e.name}</a>`
      : html`<a class="file-name"${attrs({ href: `${fileUrl(id, full)}&download=true`, "data-preview": full, "data-share": id })}><span class="file-icon" aria-hidden="true">${icon(kindOf(e.name))}</span>${e.name}</a>`;
    return {
      key: full,
      cells: [name, e.directory ? html`<span class="muted">—</span>` : fmt(e.size), html`<span class="muted nowrap">${e.modified ? ago(e.modified) : "—"}</span>`, e.directory ? "" : IconLink({ icon: "download", label: `Download ${e.name}`, href: `${fileUrl(id, full)}&download=true`, download: true, size: "sm" })],
    };
  });
  const sorts = { name: "Name", size: "Size", modified: "Modified" };
  const sorter = html`<label class="inline-select">Sort<select${attrs({ id: "file-sort", "data-share": id, "data-path": path })}>${Object.entries(sorts).map(([value, label]) => html`<option${attrs({ value, selected: sort === value })}>${label}</option>`)}</select></label>`;
  return html`${PageHeader({ title: r?.name || "Shared files", description: "Browse, preview, and download. Files are read-only here.", realm: "files" })}${Toolbar({ start: Breadcrumbs(crumbs, "Folder"), end: html`<span class="muted">${plural(listing.total, "item")}</span>${sorter}` })}${listing.entries.length ? Table({ columns: ["Name", "Size", "Modified", ""], rows }) : EmptyState({ compact: true, rune: "ᛈ", text: "This folder is empty." })}${Pager({ previous: page > 0 ? browseUrl(id, path, page - 1, sort) : "", next: (page + 1) * 100 < listing.total ? browseUrl(id, path, page + 1, sort) : "" })}`;
}

export function preview(id, path) {
  const url = fileUrl(id, path), kind = kindOf(path);
  const body = kind === "image" ? html`<img class="preview" alt="${path}" src="${url}" decoding="async">`
    : kind === "audio" ? html`<audio class="preview" controls src="${url}"></audio>`
    : kind === "video" ? html`<video class="preview" controls src="${url}"></video>`
    : kind === "text" ? html`<iframe title="File preview" class="preview-frame" sandbox src="${url}"></iframe>`
    : EmptyState({ compact: true, rune: "ᛈ", text: "Download this file to open it." });
  modal(path.split("/").pop(), html`${body}${FormFooter(LinkButton({ label: "Download original", href: `${url}&download=true`, download: true, icon: "download", kind: "primary" }), { divider: false })}`);
}
