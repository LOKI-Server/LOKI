// LOKI's components. Each takes an options object and returns safe Html (see html.js).
// Pages compose these instead of writing markup and class names by hand, so a
// button, card or field looks and behaves the same wherever it appears.
import { attrs, cx, data, html, plural, safeUrl } from "./html.js";
import { icon } from "./icons.js";

export { html, plural };

/* Buttons. Size and look are explicit, never inferred from where a button sits. */

const KINDS = { primary: "btn-primary", secondary: "btn-secondary", ghost: "btn-ghost", danger: "btn-danger", "danger-solid": "btn-danger-solid" };
const sizeClass = size => size === "sm" ? "btn-sm" : size === "lg" ? "btn-lg" : "";
const content = (iconName, label) => html`${iconName ? icon(iconName) : ""}${label !== undefined && label !== null && label !== "" ? html`<span>${label}</span>` : ""}`;

/**
 * A button that runs an action (see actions.js) or submits its form.
 * `data` adds data-* attributes; `action` and `id` become data-action and data-id.
 */
export function Button({ label, icon: iconName, action, id, data: extra = {}, kind = "secondary", size = "md", type = "button", block = false, title, ariaLabel, disabled = false, className = "", attrs: more = {} } = {}) {
  return html`<button${attrs({ type, class: cx("btn", KINDS[kind], sizeClass(size), { "btn-block": block }, className), title, "aria-label": ariaLabel, disabled, ...more })}${data({ action, id, ...extra })}>${content(iconName, label)}</button>`;
}

/** The primary button of a form. */
export const Submit = ({ label, icon: iconName, kind = "primary", size = "md", block = false, className = "" } = {}) =>
  Button({ label, icon: iconName, kind, size, block, className, type: "submit" });

/** A square button with only an icon. `label` is read by screen readers and shown on hover. */
export const IconButton = ({ icon: iconName, label, action, id, data: extra, kind = "ghost", size = "md", className = "", attrs: more = {} } = {}) =>
  Button({ icon: iconName, action, id, data: extra, kind, size, title: label, ariaLabel: label, className: cx("btn-icon", className), attrs: more });

/** A link that looks like a button. External links open in a new tab without access to this one. */
export function LinkButton({ label, href, icon: iconName, kind = "secondary", size = "md", external = false, download = false, block = false, title, ariaLabel, className = "" } = {}) {
  return html`<a${attrs({ class: cx("btn", KINDS[kind], sizeClass(size), { "btn-block": block }, className), href: safeUrl(href), target: external ? "_blank" : null, rel: external ? "noopener noreferrer" : null, download, title, "aria-label": ariaLabel })}>${content(iconName, label)}</a>`;
}

/** An icon-only link, such as a download. */
export const IconLink = ({ icon: iconName, label, href, download = false, external = false, size = "md" } = {}) =>
  LinkButton({ icon: iconName, href, download, external, size, kind: "ghost", title: label, ariaLabel: label, className: "btn-icon" });

/** A row of buttons. */
export const ButtonGroup = (...buttons) => html`<div class="btn-group">${buttons}</div>`;

/* Labels and status. */

/** A small label. Tones: accent, neutral, warn, danger, info. */
export const Badge = (label, tone = "accent") => html`<span class="${cx("badge", `badge-${tone}`)}">${label}</span>`;

/** A label with a coloured dot. Tones: ok, neutral, live (pulses), warn, danger. */
export const Status = (label, tone = "ok") => html`<span class="${cx("status", `status-${tone}`)}"><span class="status-dot"></span>${label}</span>`;

/** A count in a pill, for tabs and section headings. */
export const Count = n => n || n === 0 ? html`<span class="count">${n}</span>` : "";

/* Page structure. */

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
  sync: ["ᛇ", "Eihwaz", "the yew that endures"],
  repos: ["ᚨ", "Ansuz", "the word passed on"],
  archive: ["ᛜ", "Ingwaz", "what is stored away"],
  waiting: ["ᛁ", "Isa", "stillness before spring"],
  lost: ["ᚺ", "Hagalaz", "the storm"],
};

/** The title block at the top of a page. */
export function PageHeader({ title, description, actions, realm = "overview" }) {
  const [rune, name, meaning] = runes[realm] || runes.overview;
  return html`<header class="page-header"><div class="page-heading"><div class="eyebrow" title="${`${name}: ${meaning}`}"><span class="eyebrow-rune" aria-hidden="true">${rune}</span>${name}</div><h1>${title}</h1>${description ? html`<p>${description}</p>` : ""}</div>${actions ? html`<div class="page-actions">${actions}</div>` : ""}</header>`;
}

/** A heading over one group on a page, with an optional count, link and actions. */
export function SectionHead({ title, count, href, linkLabel = "View all", actions }) {
  return html`<div class="section-head"><h2>${title}${count !== undefined ? Count(count) : ""}</h2>${actions || href ? html`<div class="section-tools">${actions}${href ? html`<a class="section-link" href="${safeUrl(href)}">${linkLabel}${icon("chevron")}</a>` : ""}</div>` : ""}</div>`;
}

/**
 * A panel. The header holds the title (and a line under it) on the left and
 * small actions on the right, vertically centred.
 */
export function Card({ title, description, actions, body, id, className = "", flush = false, tag = "section" } = {}) {
  const head = title || actions ? html`<header class="card-head"><div class="card-title">${title ? html`<h2>${title}</h2>` : ""}${description ? html`<p>${description}</p>` : ""}</div>${actions ? html`<div class="card-actions">${actions}</div>` : ""}</header>` : "";
  const open = html`<${tag === "article" ? "article" : "section"}${attrs({ class: cx("card", { "card-flush": flush }, className), id })}>`;
  const hasBody = body !== undefined && body !== null && body !== "" && !(Array.isArray(body) && !body.length);
  return html`${open}${head}${hasBody ? html`<div class="card-body">${body}</div>` : ""}</${tag === "article" ? "article" : "section"}>`;
}

/** Columns: a wide main column and a narrower side column that stack on small screens. */
export const Split = (main, side) => html`<div class="split"><div class="stack">${main}</div><div class="stack">${side}</div></div>`;
export const Stack = (...items) => html`<div class="stack">${items}</div>`;

/** A toolbar: things on the left, things on the right. */
export const Toolbar = ({ start, end } = {}) => html`<div class="toolbar"><div class="toolbar-start">${start}</div>${end ? html`<div class="toolbar-end">${end}</div>` : ""}</div>`;

/** A breadcrumb trail. Items are {label, href, icon}; the last one is the current place. */
export function Breadcrumbs(items, label = "Location") {
  return html`<nav class="breadcrumbs" aria-label="${label}">${items.map((item, i) => {
    const last = i === items.length - 1;
    const text = html`${item.icon ? icon(item.icon) : ""}${item.label}`;
    return html`${i ? icon("chevron", "crumb-sep") : ""}${last && !item.href ? html`<span aria-current="page">${text}</span>` : html`<a href="${safeUrl(item.href)}">${text}</a>`}`;
  })}</nav>`;
}

/** Previous and next links; either may be missing. */
export function Pager({ previous, next, previousLabel = "Previous", nextLabel = "Next" }) {
  if (!previous && !next) return "";
  return html`<div class="pager">${previous ? LinkButton({ label: previousLabel, href: previous, icon: "left", size: "sm" }) : ""}${next ? LinkButton({ label: nextLabel, href: next, icon: "chevron", size: "sm" }) : ""}</div>`;
}

/* Lists and tables. */

/** Rows inside a bordered box. */
export const List = (rows, { className = "", empty } = {}) => {
  const items = [rows].flat().filter(Boolean);
  if (!items.length && empty) return html`<p class="list-empty">${empty}</p>`;
  return html`<div class="${cx("list", className)}">${items}</div>`;
};

/**
 * One row: a leading icon or avatar, a title (optionally a link) with badges,
 * a line of detail, and actions at the end.
 */
export function Row({ key, icon: iconName, tone = "", lead, title, href, external = false, badges, meta, actions, className = "" }) {
  const leading = lead || (iconName ? html`<span class="${cx("row-icon", tone && `tone-${tone}`)}" aria-hidden="true">${icon(iconName)}</span>` : "");
  const name = href ? html`<a class="row-title"${attrs({ href: safeUrl(href), target: external ? "_blank" : null, rel: external ? "noopener noreferrer" : null })}>${title}</a>` : html`<span class="row-title">${title}</span>`;
  return html`<div${attrs({ class: cx("row", className), "data-key": key })}>${leading}<div class="row-body"><div class="row-line">${name}${badges}</div>${meta ? html`<div class="row-meta">${meta}</div>` : ""}</div>${actions ? html`<div class="row-actions">${actions}</div>` : ""}</div>`;
}

/** A table whose rows become labelled cards on narrow screens. Cells are Html or text. */
export function Table({ columns, rows, empty = "" }) {
  if (!rows.length && empty) return html`<p class="list-empty">${empty}</p>`;
  return html`<div class="table-wrap"><table><thead><tr>${columns.map(c => html`<th>${c}</th>`)}</tr></thead><tbody>${rows.map(({ key, cells }) => html`<tr${attrs({ "data-key": key })}>${cells.map((cell, i) => html`<td data-label="${columns[i]}">${cell}</td>`)}</tr>`)}</tbody></table></div>`;
}

/** Labelled values. */
export const KeyValue = rows => html`<dl class="kv">${rows.filter(Boolean).map(([label, value]) => html`<div class="kv-row"><dt>${label}</dt><dd>${value === undefined || value === null || value === "" ? html`<span class="kv-none">None</span>` : value}</dd></div>`)}</dl>`;

/* Messages. */

/** A callout. Tones: info, warn, danger, ok. `action` sits at the end. */
export function Notice({ tone = "info", icon: iconName, title, body, action, className = "" }) {
  const glyph = iconName ?? { warn: "alert", danger: "alert", ok: "check", info: "" }[tone];
  return html`<div class="${cx("notice", `notice-${tone}`, className)}"${attrs({ role: tone === "danger" ? "alert" : null })}>${glyph ? icon(glyph) : ""}<div class="notice-body">${title ? html`<strong>${title}</strong>` : ""}${body}</div>${action ? html`<div class="notice-action">${action}</div>` : ""}</div>`;
}

/** What a page or box shows when there is nothing in it. */
export function EmptyState({ title, text, action, rune = "ᛉ", compact = false }) {
  return html`<div class="${cx("empty", { "empty-compact": compact })}"><span class="empty-rune" aria-hidden="true">${rune}</span>${title ? html`<h2>${title}</h2>` : ""}${text ? html`<p>${text}</p>` : ""}${action ? html`<div class="empty-actions">${action}</div>` : ""}</div>`;
}

/** A dashed row suggesting a first step. */
export const Hint = ({ text, action, icon: iconName = "plus" }) => html`<div class="hint">${icon(iconName)}<span>${text}</span>${action}</div>`;

export const Help = text => html`<p class="help">${text}</p>`;

/* Forms. Every control has a label; ids are unique per render. */

let fieldCounter = 0;
const fieldId = name => `f-${String(name).replace(/[^a-z0-9_-]/gi, "")}-${++fieldCounter}`;
const helpText = help => help ? html`<p class="help">${help}</p>` : "";

/** A labelled input. Extra input attributes go in `attrs` or as the common keys below. */
export function Field({ name, label, value = "", type = "text", help, required, placeholder, min, max, minlength, maxlength, pattern, autocomplete, inputmode, readonly, hiddenLabel = false, attrs: more = {} }) {
  const id = fieldId(name);
  return html`<div class="field"><label${attrs({ for: id, class: hiddenLabel ? "visually-hidden" : null })}>${label}</label><input${attrs({ id, name, type, value: value ?? "", required, placeholder, min, max, minlength, maxlength, pattern, autocomplete, inputmode, readonly, ...more })}>${helpText(help)}</div>`;
}

/** A labelled select. Options are [value, label] pairs or plain strings. */
export function Select({ name, label, options, value, help, id, hiddenLabel = false, attrs: more = {} }) {
  const fid = id || fieldId(name);
  return html`<div class="field"><label${attrs({ for: fid, class: hiddenLabel ? "visually-hidden" : null })}>${label}</label><select${attrs({ id: fid, name, ...more })}>${options.map(option => {
    const [v, text] = Array.isArray(option) ? option : [option, option];
    return html`<option${attrs({ value: v, selected: String(v) === String(value) })}>${text}</option>`;
  })}</select>${helpText(help)}</div>`;
}

/** A checkbox with its label beside it. */
export function Checkbox({ name, label, checked = false, help, attrs: more = {} }) {
  return html`<div class="field field-check"><label class="check"><input${attrs({ type: "checkbox", name, checked, ...more })}><span>${label}</span></label>${helpText(help)}</div>`;
}

/** A labelled multi-line text box. `mono` uses the code font. */
export function TextArea({ name, label, value = "", rows, help, placeholder, maxlength, required, mono = false, hiddenLabel = false, attrs: more = {} }) {
  const id = fieldId(name);
  return html`<div class="field"><label${attrs({ for: id, class: hiddenLabel ? "visually-hidden" : null })}>${label}</label><textarea${attrs({ id, name, rows, placeholder, maxlength, required, spellcheck: mono ? "false" : null, class: mono ? "mono-input" : null, ...more })}>${value}</textarea>${helpText(help)}</div>`;
}

/** A labelled file picker. */
export const FileInput = ({ name, label, accept, required = true, help, hiddenLabel = false }) => {
  const id = fieldId(name);
  return html`<div class="field"><label${attrs({ for: id, class: hiddenLabel ? "visually-hidden" : null })}>${label}</label><input${attrs({ id, name, type: "file", accept, required })}>${helpText(help)}</div>`;
};

/** Two fields side by side. */
export const FieldRow = (...fields) => html`<div class="field-row">${fields}</div>`;

/** Buttons at the bottom of a form. `align` is "end" (default), "start" or "between". */
export const FormFooter = (buttons, { align = "end", divider = true } = {}) =>
  html`<div class="${cx("form-footer", `align-${align}`, { "no-divider": !divider })}">${buttons}</div>`;

/** A form handled by actions.js (data-form names the handler). */
export function Form({ name, id, body, className = "", data: extra = {}, attrs: more = {} }) {
  return html`<form${attrs({ class: className || null, ...more })}${data({ form: name, id, ...extra })}>${body}</form>`;
}

/** A titled part of a dialog. */
export const DialogSection = (title, body) => html`<section class="dialog-section"><h3>${title}</h3>${body}</section>`;

/* Navigation widgets. */

/** Underlined tabs. Each item is {label, href, active, count, icon}. */
export function Tabs(items, label = "Sections") {
  return html`<nav class="tabs" aria-label="${label}">${items.filter(Boolean).map(t => html`<a${attrs({ href: safeUrl(t.href), class: t.active ? "active" : null, "aria-current": t.active ? "page" : null })}>${t.icon ? icon(t.icon) : ""}<span>${t.label}</span>${t.count ? Count(t.count) : ""}</a>`)}</nav>`;
}

/** A dropdown. `trigger` is the button's content; it closes on outside clicks (actions.js). */
export function Menu({ trigger, body, kind = "secondary", size = "md", align = "left", label }) {
  return html`<details class="${cx("menu", `menu-${align}`)}"><summary${attrs({ class: cx("btn", KINDS[kind], sizeClass(size)), "aria-label": label })}>${trigger}${icon("down", "menu-caret")}</summary><div class="menu-panel">${body}</div></details>`;
}

/** Links inside a Menu. Items are {label, href, current, badge}. */
export const MenuList = (title, items) => html`${title ? html`<h4 class="menu-title">${title}</h4>` : ""}<div class="menu-list">${items.map(item => html`<a${attrs({ href: safeUrl(item.href), "aria-current": item.current ? "true" : null })}>${icon("check", "menu-tick")}<span>${item.label}</span>${item.badge}</a>`)}</div>`;

/** Text with a copy button beside it. */
export const CopyField = (text, label = "Copy") => html`<div class="copy-field"><code>${text}</code>${IconButton({ icon: "copy", label, action: "copy", data: { copy: text }, size: "sm" })}</div>`;

/** A block of commands with a copy button in its corner. */
export const CopyBlock = (text, label = "Copy") => html`<div class="copy-block"><pre>${text}</pre>${IconButton({ icon: "copy", label, action: "copy", data: { copy: text }, size: "sm" })}</div>`;

/** How full something is. */
export const Meter = ({ value, max, label }) => html`<progress class="meter"${attrs({ max: Math.max(1, max), value: Math.min(value, max), "aria-label": label })}></progress>`;

/** A loading placeholder for the whole page. */
export const Skeleton = () => html`<div class="skeleton-page" aria-label="Loading"><div class="sk sk-eyebrow"></div><div class="sk sk-title"></div><div class="sk sk-line"></div><div class="sk-grid"><div class="sk sk-card"></div><div class="sk sk-card"></div><div class="sk sk-card"></div></div></div>`;

export function initial(name) { return String(name || "?").trim().charAt(0).toUpperCase() || "?"; }

/** Readable view of a JSON value, with the raw JSON one click away. */
export function jsonView(value) {
  return html`<div class="kv-view">${kv(value, 0)}</div><details class="disclosure"><summary>Raw JSON</summary><pre>${JSON.stringify(value, null, 2)}</pre></details>`;
}
const keyLabel = key => String(key).replace(/[_-]+/g, " ").replace(/^\w/, c => c.toUpperCase());
function kv(value, depth) {
  if (value === null || value === undefined || value === "") return html`<span class="kv-none">—</span>`;
  if (typeof value === "boolean") return Badge(value ? "Yes" : "No", value ? "accent" : "neutral");
  if (typeof value === "number") return html`<span class="kv-num">${value}</span>`;
  if (typeof value !== "object") return html`<span class="kv-str">${value}</span>`;
  if (Array.isArray(value)) {
    if (!value.length) return html`<span class="kv-none">None</span>`;
    if (value.every(v => v === null || typeof v !== "object")) return html`<ul class="kv-list">${value.map(v => html`<li>${kv(v, depth + 1)}</li>`)}</ul>`;
    return value.map((v, i) => html`<details class="kv-group"${attrs({ open: depth < 1 })}><summary>Item ${i + 1}</summary>${kv(v, depth + 1)}</details>`);
  }
  const entries = Object.entries(value);
  if (!entries.length) return html`<span class="kv-none">Empty</span>`;
  return html`<dl class="kv">${entries.map(([key, v]) => {
    const nested = v && typeof v === "object" && depth >= 1;
    const summary = Array.isArray(v) ? plural(v.length, "item") : v && plural(Object.keys(v).length, "field");
    return html`<div class="kv-row"><dt>${keyLabel(key)}</dt><dd>${nested ? html`<details class="kv-group"><summary>${summary}</summary>${kv(v, depth + 1)}</details>` : kv(v, depth + 1)}</dd></div>`;
  })}</dl>`;
}
