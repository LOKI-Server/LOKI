// Browsing and previewing a file share. Folder, page, and sort live in the URL,
// so Back, reload, and shared links all land in the same place.
import { api, ago, enc, esc, fmt, modal, resource } from "../lib/core.js";
import { icon } from "../lib/icons.js";
import { heading, linkButton, table } from "../lib/ui.js";

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
  const crumbs = path.split("/").filter(Boolean).map(part => {
    prefix += (prefix ? "/" : "") + part;
    return `${icon("chevron", "crumb-sep")}<a href="${esc(browseUrl(id, prefix, 0, sort))}">${esc(part)}</a>`;
  }).join("");
  const rows = listing.entries.map(e => {
    const full = path ? `${path}/${e.name}` : e.name;
    const name = e.directory
      ? `<a class="file-name" href="${esc(browseUrl(id, full, 0, sort))}"><span class="file-icon folder" aria-hidden="true">${icon("folder")}</span>${esc(e.name)}</a>`
      : `<a class="file-name" href="${esc(fileUrl(id, full))}&download=true" data-preview="${esc(full)}" data-share="${esc(id)}"><span class="file-icon" aria-hidden="true">${icon(kindOf(e.name))}</span>${esc(e.name)}</a>`;
    return {
      key: full,
      cells: [name, e.directory ? '<span class="muted">—</span>' : fmt(e.size), `<span class="file-meta">${e.modified ? ago(e.modified) : "—"}</span>`, e.directory ? "" : `<a class="icon-link" href="${esc(fileUrl(id, full))}&download=true" download aria-label="Download ${esc(e.name)}">${icon("download")}</a>`],
    };
  });
  const sorts = { name: "Name", size: "Size", modified: "Modified" };
  const pager = [
    page > 0 ? linkButton("Previous", browseUrl(id, path, page - 1, sort), "subtle", "left") : "",
    (page + 1) * 100 < listing.total ? linkButton("Next", browseUrl(id, path, page + 1, sort), "subtle", "chevron") : "",
  ].join("");
  return heading(r?.name || "Shared files", "Browse, preview, and download. Files are read-only here.", "", "files")
    + `<div class="toolbar"><nav class="breadcrumbs" aria-label="Folder"><a href="${esc(browseUrl(id, "", 0, sort))}">${icon("folder")}${esc(r?.name || "Files")}</a>${crumbs}</nav><div class="toolbar-end"><span class="muted">${listing.total} item${listing.total === 1 ? "" : "s"}</span><label class="inline-label">Sort <select id="file-sort" data-share="${esc(id)}" data-path="${esc(path)}">${Object.entries(sorts).map(([value, label]) => `<option value="${value}" ${sort === value ? "selected" : ""}>${label}</option>`).join("")}</select></label></div></div>`
    + (listing.entries.length ? table(["Name", "Size", "Modified", ""], rows) : '<div class="empty small"><span class="empty-rune" aria-hidden="true">ᛈ</span><p>This folder is empty.</p></div>')
    + (pager ? `<div class="actions pager">${pager}</div>` : "");
}

export function preview(id, path) {
  const url = fileUrl(id, path), kind = kindOf(path);
  const body = kind === "image" ? `<img class="preview" alt="${esc(path)}" src="${url}" decoding="async">`
    : kind === "audio" ? `<audio class="preview" controls src="${url}"></audio>`
    : kind === "video" ? `<video class="preview" controls src="${url}"></video>`
    : kind === "text" ? `<iframe title="File preview" class="preview-frame" sandbox src="${url}"></iframe>`
    : `<div class="empty small">${icon("file", "big")}<p>Download this file to open it.</p></div>`;
  modal(path.split("/").pop(), body + `<div class="actions"><a class="button" href="${url}&download=true" download>${icon("download")}Download original</a></div>`);
}
