// App panels: an app describes its controls and LOKI draws them here, with its own
// components. Every string from the app is escaped; nothing the app sends runs.
import { api, enc, esc, modal, resource, toast } from "../lib/core.js";
import { button, check, field, tag } from "../lib/ui.js";

const styles = { primary: "primary", danger: "danger", subtle: "subtle" };

function action(app, a) {
  const extra = `data-name="${esc(a.action)}" data-value="${esc(a.value || "")}" data-confirm="${esc(a.confirm || "")}" data-label="${esc(a.label)}"`;
  return button(a.label, "panel-action", app, extra, styles[a.style] || "subtle");
}

function input(f) {
  const value = f.value ?? "";
  if (f.kind === "bool") return check(f.key, f.label, value === true || value === "true");
  if (f.kind === "select") {
    const options = (f.options || []).map(([v, label]) => `<option value="${esc(v)}" ${String(value) === v ? "selected" : ""}>${esc(label)}</option>`).join("");
    return `<label>${esc(f.label)}<select name="${esc(f.key)}">${options}</select></label>`;
  }
  return field(f.key, f.label, String(value), f.kind === "number" ? "number" : "text");
}

function item(app, i) {
  switch (i.type) {
    case "text": return `<p class="${i.tone === "muted" ? "help" : i.tone === "warning" ? "notice" : ""}">${esc(i.text)}</p>`;
    case "stats": return `<div class="panel-stats">${i.items.map(s => `<div><span>${esc(s.label)}</span><strong>${esc(s.value)}</strong></div>`).join("")}</div>`;
    case "list":
      if (!i.items.length) return `<p class="help">${esc(i.empty || "Nothing here yet.")}</p>`;
      return `<div class="session-list">${i.items.map(r => `<div class="session-row"><div><strong>${esc(r.title)}</strong>${r.badge ? tag(r.badge) : ""}${r.detail ? `<small>${esc(r.detail)}</small>` : ""}</div>${r.actions?.length ? `<div class="actions">${r.actions.map(a => action(app, a)).join("")}</div>` : ""}</div>`).join("")}</div>`;
    case "upload": return `<form class="panel-upload" data-form="panel-upload" data-id="${esc(app)}" data-name="${esc(i.action)}"><label>${esc(i.label || "Choose a file")}<input type="file" name="file" accept="${esc(i.accept || "")}" required></label><div class="actions"><button>${esc(i.button || "Upload")}</button></div></form>`;
    case "buttons": return `<div class="actions">${i.items.map(a => action(app, a)).join("")}</div>`;
    case "form": return `<form data-form="panel-form" data-id="${esc(app)}" data-name="${esc(i.action)}">${i.fields.map(input).join("")}<div class="actions"><button>${esc(i.button || "Save")}</button></div></form>`;
    case "code": return `${i.label ? `<p class="help">${esc(i.label)}</p>` : ""}<pre class="secret">${esc(i.text)}</pre>`;
    default: return "";
  }
}

/** Draws a panel the app returned, with its error above and its notice as a toast. */
export function showPanel(app, result) {
  const sections = result.panel.sections.map(s => `<section class="dialog-section panel-section">${s.title ? `<h3>${esc(s.title)}</h3>` : ""}${s.items.map(i => item(app, i)).join("")}</section>`).join("");
  modal(resource(app)?.name || "App", `${result.error ? `<p class="notice error">${esc(result.error)}</p>` : ""}${sections || '<p class="help">This app has nothing to show yet.</p>'}`);
  if (result.notice) toast(result.notice);
}

export async function panelDialog(app) {
  showPanel(app, await api(`/api/apps/${enc(app)}/panel`));
}

/** Sends one action: a button press, a form's fields, or an upload. */
export async function panelAction(app, body) {
  showPanel(app, await api(`/api/apps/${enc(app)}/panel`, body));
}
