// App panels: an app describes its controls and LOKI draws them here, with its own
// components. Everything the app sends is treated as text; nothing it sends runs.
import { api, enc, modal, resource, toast } from "../lib/core.js";
import { html } from "../lib/html.js";
import { Badge, Button, ButtonGroup, Checkbox, Field, FileInput, Form, FormFooter, Help, List, Notice, Row, Select, Submit } from "../lib/ui.js";

const styles = { primary: "primary", danger: "danger", subtle: "secondary" };

function action(app, a) {
  return Button({ label: a.label, action: "panel-action", id: app, kind: styles[a.style] || "secondary", size: "sm", data: { name: a.action, value: a.value || "", confirm: a.confirm || "", label: a.label } });
}

function input(f) {
  const value = f.value ?? "";
  if (f.kind === "bool") return Checkbox({ name: f.key, label: f.label, checked: value === true || value === "true" });
  if (f.kind === "select") return Select({ name: f.key, label: f.label, options: (f.options || []).map(([v, label]) => [String(v), label]), value: String(value) });
  return Field({ name: f.key, label: f.label, value: String(value), type: f.kind === "number" ? "number" : "text" });
}

function item(app, i) {
  switch (i.type) {
    case "text": return i.tone === "warning" ? Notice({ tone: "warn", body: i.text }) : html`<p class="${i.tone === "muted" ? "help" : ""}">${i.text}</p>`;
    case "stats": return html`<div class="panel-stats">${(i.items || []).map(s => html`<div><span>${s.label}</span><strong>${s.value}</strong></div>`)}</div>`;
    case "list":
      if (!i.items?.length) return Help(i.empty || "Nothing here yet.");
      return List(i.items.map(r => Row({ title: r.title, badges: r.badge ? Badge(r.badge, "neutral") : "", meta: r.detail || "", actions: r.actions?.length ? r.actions.map(a => action(app, a)) : "" })));
    case "upload": return Form({ name: "panel-upload", id: app, data: { name: i.action }, body: html`${FileInput({ name: "file", label: i.label || "Choose a file", accept: i.accept || "" })}${FormFooter(Submit({ label: i.button || "Upload" }), { divider: false })}` });
    case "buttons": return ButtonGroup(...(i.items || []).map(a => action(app, a)));
    case "form": return Form({ name: "panel-form", id: app, data: { name: i.action }, body: html`${(i.fields || []).map(input)}${FormFooter(Submit({ label: i.button || "Save" }), { divider: false })}` });
    case "code": return html`${i.label ? Help(i.label) : ""}<pre class="secret">${i.text}</pre>`;
    default: return "";
  }
}

/** Draws a panel the app returned, with its error above and its notice as a toast. */
export function showPanel(app, result) {
  const sections = (result.panel?.sections || []).map(s => html`<section class="dialog-section">${s.title ? html`<h3>${s.title}</h3>` : ""}${(s.items || []).map(i => item(app, i))}</section>`);
  modal(resource(app)?.name || "App", html`${result.error ? Notice({ tone: "danger", body: result.error }) : ""}${sections.length ? sections : Help("This app has nothing to show yet.")}`);
  if (result.notice) toast(result.notice);
}

export async function panelDialog(app) {
  showPanel(app, await api(`/api/apps/${enc(app)}/panel`));
}

/** Sends one action: a button press, a form's fields, or an upload. */
export async function panelAction(app, body) {
  showPanel(app, await api(`/api/apps/${enc(app)}/panel`, body));
}
