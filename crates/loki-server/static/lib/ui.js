// HTML building blocks shared by every page. Each helper escapes what it is given.
import { esc } from "./core.js";
import { icon } from "./icons.js";

let fieldCounter = 0;
export const field = (name, label, value = "", type = "text", extra = "") => {
  const id = `f-${name}-${++fieldCounter}`;
  return `<label for="${esc(id)}">${esc(label)}</label><input id="${esc(id)}" name="${esc(name)}" type="${type}" value="${esc(value)}" ${extra}>`;
};
export const check = (name, label, on = false) => `<label class="check"><input type="checkbox" name="${esc(name)}" ${on ? "checked" : ""}><span>${esc(label)}</span></label>`;
export const button = (label, action, id = "", extra = "", kind = "subtle", iconName = "") =>
  `<button type="button" class="${kind}" data-action="${action}" data-id="${esc(id)}" ${extra}>${iconName ? icon(iconName) : ""}${esc(label)}</button>`;
export const linkButton = (label, href, kind = "subtle", iconName = "", extra = "") =>
  `<a class="button ${kind}" href="${esc(href)}" ${extra}>${iconName ? icon(iconName) : ""}${esc(label)}</a>`;
export const tag = (label, kind = "") => `<span class="tag ${kind}">${esc(label)}</span>`;
/** A status pill with a coloured dot; "live" pulses while something is starting. */
export const status = (label, kind = "") => `<span class="tag status ${kind}"><span class="dot"></span>${esc(label)}</span>`;

// Each page has a rune from the Elder Futhark, chosen for what the page holds.
const runes = {
  overview: ["ᛟ", "Othala", "home and inheritance"],
  apps: ["ᚲ", "Kenaz", "the torch of craft"],
  shares: ["ᚠ", "Fehu", "wealth that is shared"],
  files: ["ᛈ", "Perthro", "what lies within"],
  routes: ["ᚱ", "Raidho", "the road and the journey"],
  people: ["ᛗ", "Mannaz", "the people"],
  activity: ["ᛞ", "Dagaz", "day and change"],
  settings: ["ᛏ", "Tiwaz", "order and law"],
  account: ["ᛖ", "Ehwaz", "trust between companions"],
  waiting: ["ᛁ", "Isa", "stillness before spring"],
  lost: ["ᚺ", "Hagalaz", "the storm"],
};
export function heading(title, description, actions = "", realm = "overview") {
  const [rune, name, meaning] = runes[realm] || runes.overview;
  return `<div class="page-header"><div><div class="eyebrow" title="${esc(`${name}: ${meaning}`)}"><span class="eyebrow-rune" aria-hidden="true">${rune}</span>${esc(name)}</div><h1>${esc(title)}</h1><p>${esc(description)}</p></div>${actions ? `<div class="actions">${actions}</div>` : ""}</div>`;
}
export function empty(title, text, action = "", rune = "ᛉ") {
  return `<div class="empty"><span class="empty-rune" aria-hidden="true">${rune}</span><h2>${esc(title)}</h2><p>${esc(text)}</p>${action}</div>`;
}
export const skeleton = () => `<div class="skeleton-page" aria-label="Loading"><div class="sk sk-eyebrow"></div><div class="sk sk-title"></div><div class="sk sk-line"></div><div class="sk-grid"><div class="sk sk-card"></div><div class="sk sk-card"></div><div class="sk sk-card"></div></div></div>`;

/** A table whose rows become labelled cards on narrow screens. Cells are HTML. */
export function table(head, rows, empty = "") {
  if (!rows.length && empty) return `<p class="notice">${esc(empty)}</p>`;
  return `<div class="table-wrap"><table><thead><tr>${head.map(h => `<th>${esc(h)}</th>`).join("")}</tr></thead><tbody>${rows.map(({ key, cells }) => `<tr${key ? ` data-key="${esc(key)}"` : ""}>${cells.map((cell, i) => `<td data-label="${esc(head[i])}">${cell}</td>`).join("")}</tr>`).join("")}</tbody></table></div>`;
}

export function initial(name) { return esc(String(name || "?").trim().charAt(0).toUpperCase() || "?"); }

/** Readable view of a JSON value, with the raw JSON one click away. */
export function jsonView(value) {
  return `<div class="kv-view">${kv(value, 0)}</div><details class="raw"><summary>Raw JSON</summary><pre>${esc(JSON.stringify(value, null, 2))}</pre></details>`;
}
const label = key => String(key).replace(/[_-]+/g, " ").replace(/^\w/, c => c.toUpperCase());
const count = (n, noun) => `${n} ${noun}${n === 1 ? "" : "s"}`;
function kv(value, depth) {
  if (value === null || value === undefined || value === "") return '<span class="kv-none">—</span>';
  if (typeof value === "boolean") return tag(value ? "Yes" : "No", value ? "" : "dim");
  if (typeof value === "number") return `<span class="kv-num">${value}</span>`;
  if (typeof value !== "object") return `<span class="kv-str">${esc(value)}</span>`;
  if (Array.isArray(value)) {
    if (!value.length) return '<span class="kv-none">None</span>';
    if (value.every(v => v === null || typeof v !== "object")) return `<ul class="kv-list">${value.map(v => `<li>${kv(v, depth + 1)}</li>`).join("")}</ul>`;
    return value.map((v, i) => `<details class="kv-group"${depth < 1 ? " open" : ""}><summary>Item ${i + 1}</summary>${kv(v, depth + 1)}</details>`).join("");
  }
  const entries = Object.entries(value);
  if (!entries.length) return '<span class="kv-none">Empty</span>';
  return `<dl class="kv">${entries.map(([key, v]) => {
    const nested = v && typeof v === "object" && depth >= 1;
    const summary = Array.isArray(v) ? count(v.length, "item") : v && count(Object.keys(v).length, "field");
    return `<div class="kv-row"><dt>${esc(label(key))}</dt><dd>${nested ? `<details class="kv-group"><summary>${summary}</summary>${kv(v, depth + 1)}</details>` : kv(v, depth + 1)}</dd></div>`;
  }).join("")}</dl>`;
}
