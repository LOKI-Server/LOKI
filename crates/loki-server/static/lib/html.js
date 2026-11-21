// Safe HTML. Every value placed in an html`` template is escaped unless it is itself
// the result of html`` (or raw(), for markup this code wrote). Forgetting to escape
// server data is therefore impossible: the worst case is visible markup, never script.

const ENTITIES = { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" };
export const escape = value => String(value ?? "").replace(/[&<>"']/g, c => ENTITIES[c]);

/** Markup that is already safe to insert. Only html``, raw() and the components make these. */
export class Html {
  constructor(text) { this.text = text; }
  toString() { return this.text; }
  // `a + b` would turn markup into a plain string that later gets escaped. Fail loudly instead.
  [Symbol.toPrimitive](hint) {
    if (hint === "default") throw new TypeError("Join Html with html`` or an array, not +.");
    return this.text;
  }
}

/** Trusted markup written in this codebase (SVG paths, fixed strings). Never pass server data. */
export const raw = text => new Html(String(text ?? ""));
export const isHtml = value => value instanceof Html;

function piece(value) {
  if (value === null || value === undefined || value === false || value === true) return "";
  if (value instanceof Html) return value.text;
  if (Array.isArray(value)) return value.map(piece).join("");
  return escape(value);
}

/** Tagged template: html`<p>${name}</p>`. Arrays are joined; null, undefined and booleans vanish. */
export function html(strings, ...values) {
  let out = strings[0];
  for (let i = 0; i < values.length; i++) out += piece(values[i]) + strings[i + 1];
  return new Html(out);
}

/** Attributes from an object: {href: x, hidden: true, title: null} → ` href="x" hidden`. */
export function attrs(map = {}) {
  let out = "";
  for (const [name, value] of Object.entries(map)) {
    if (value === null || value === undefined || value === false) continue;
    if (!/^[a-z][a-z0-9-]*$/i.test(name)) throw new Error(`Unsafe attribute name: ${name}`);
    out += value === true ? ` ${name}` : ` ${name}="${escape(value)}"`;
  }
  return new Html(out);
}

/** `data-*` attributes from an object, so callers never hand-build attribute strings. */
export const data = (map = {}) => attrs(Object.fromEntries(Object.entries(map).map(([key, value]) => [`data-${key.replace(/[A-Z]/g, c => `-${c.toLowerCase()}`)}`, value])));

/** Class names from strings and {name: condition} objects. */
export function cx(...parts) {
  const names = [];
  for (const part of parts) {
    if (!part) continue;
    if (typeof part === "string") names.push(part);
    else for (const [name, on] of Object.entries(part)) if (on) names.push(name);
  }
  return names.join(" ");
}

/**
 * A link target that cannot run script. Relative links and http(s), mailto: and
 * tel: URLs pass; anything else (javascript:, data:, vbscript:…) becomes "#".
 */
export function safeUrl(url) {
  const text = String(url ?? "").trim();
  if (!text) return "#";
  if (/^(\/(?!\/)|\.{0,2}\/|\?|#)/.test(text)) return text;
  try {
    const parsed = new URL(text, "http://relative.invalid");
    if (parsed.origin === "http://relative.invalid") return text;
    return ["http:", "https:", "mailto:", "tel:"].includes(parsed.protocol) ? text : "#";
  } catch { return "#"; }
}

export const plural = (n, one, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;
