// Shared state, server access, dialogs, and DOM patching.
// All server-supplied text is escaped before it reaches innerHTML.
import { icon } from "./icons.js";

export const store = { session: null, state: null, loadedAt: 0, avatarVersion: 0 };

export const $ = (selector, root = document) => root.querySelector(selector);
export const esc = value => String(value ?? "").replace(/[&<>"']/g, c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
export const enc = encodeURIComponent;
export const fmt = n => n < 1024 ? `${n} B` : n < 1048576 ? `${(n / 1024).toFixed(1)} KB` : n < 1073741824 ? `${(n / 1048576).toFixed(1)} MB` : `${(n / 1073741824).toFixed(1)} GB`;
export const date = seconds => new Date(seconds * 1000).toLocaleString();
export const resource = id => store.state?.resources?.find(r => r.id === id);
export const reducedMotion = () => globalThis.matchMedia?.("(prefers-reduced-motion: reduce)").matches ?? true;

const units = [["year", 31536000], ["month", 2592000], ["week", 604800], ["day", 86400], ["hour", 3600], ["minute", 60]];
/** "3 minutes ago", wrapped in <time> with the full date on hover. */
export function ago(seconds) {
  const delta = seconds - Date.now() / 1000;
  let text = "just now";
  for (const [unit, size] of units) {
    if (Math.abs(delta) >= size) {
      text = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" }).format(Math.round(delta / size), unit);
      break;
    }
  }
  return `<time datetime="${new Date(seconds * 1000).toISOString()}" title="${esc(date(seconds))}">${esc(text)}</time>`;
}

export async function api(path, body, method = "POST") {
  const options = { method: body === undefined ? "GET" : method, credentials: "same-origin", headers: {} };
  if (body !== undefined) {
    options.headers["x-csrf-token"] = store.session.csrf;
    if (body instanceof FormData) options.body = body;
    else {
      options.headers["content-type"] = "application/json";
      options.body = JSON.stringify(body);
    }
  }
  const response = await fetch(path, options);
  let value;
  try { value = await response.json(); } catch { throw new Error(`Server returned ${response.status}`); }
  if (!response.ok) throw new Error(value.error || `Request failed (${response.status})`);
  return value;
}
export const act = (action, id = "", data = {}) => api("/api/action", { action, id, data });

export function toast(message, error = false) {
  const box = $("#toast");
  box.innerHTML = `${icon(error ? "alert" : "check")}<span>${esc(message)}</span>`;
  box.className = error ? "error" : "";
  box.hidden = false;
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => box.hidden = true, error ? 12000 : 5000);
}

export function modal(title, html) {
  $("#dialog-body").innerHTML = `<h2 id="dialog-title">${esc(title)}</h2>${html}`;
  const dialog = $("#dialog");
  if (!dialog.open) dialog.showModal();
}
export function close() { $("#dialog").close(); }

/** A styled replacement for window.confirm(). Resolves true only for the confirm button. */
export function confirmDialog({ title, message, confirm = "Confirm", danger = true }) {
  const dialog = $("#confirm");
  $("#confirm-title").textContent = title;
  $("#confirm-message").textContent = message;
  const yes = $("#confirm-yes");
  yes.textContent = confirm;
  yes.className = danger ? "danger-solid" : "";
  dialog.returnValue = "";
  dialog.showModal();
  $("button.subtle", dialog).focus();
  return new Promise(resolve => dialog.addEventListener("close", () => resolve(dialog.returnValue === "yes"), { once: true }));
}

/**
 * Update `target` to match `html`, keeping nodes that did not change. Unchanged
 * nodes keep focus, scroll position, playing media, and anything typed into them:
 * attribute updates never overwrite a form control the user has edited.
 */
export function morph(target, html) {
  const next = document.createElement(target.tagName);
  next.innerHTML = html;
  patchChildren(target, next);
}
function patchChildren(from, to) {
  const before = [...from.childNodes], after = [...to.childNodes];
  after.forEach((node, i) => {
    const old = before[i];
    if (!old) from.appendChild(node);
    else if (old.nodeType !== node.nodeType || old.nodeName !== node.nodeName || (old.nodeType === 1 && old.dataset.key !== node.dataset.key)) from.replaceChild(node, old);
    else if (old.nodeType === 1) patchElement(old, node);
    else if (old.nodeValue !== node.nodeValue) old.nodeValue = node.nodeValue;
  });
  for (let i = before.length - 1; i >= after.length; i--) before[i].remove();
}
function patchElement(old, node) {
  for (const { name } of [...old.attributes]) if (!node.hasAttribute(name)) old.removeAttribute(name);
  for (const { name, value } of [...node.attributes]) if (old.getAttribute(name) !== value) old.setAttribute(name, value);
  patchChildren(old, node);
}
