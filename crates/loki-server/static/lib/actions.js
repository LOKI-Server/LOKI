// What happens when people submit forms, press buttons, and change controls.
import { $, act, api, close, confirmDialog, enc, modal, resource, store, toast } from "./core.js";
import { html } from "./html.js";
import { Button, Field, FileInput, Form, FormFooter, LinkButton, Notice, Select, Submit, jsonView } from "./ui.js";
import { navigate, refresh, routable } from "./router.js";
import { submitSetup, submitStartupAware } from "../pages/auth.js";
import { appManage, folderPicker, vcsManage, vcsLimits, vcsQuotaDialog, grantRow, roles, routeForm, routeManage, shareForm, shareManage, subjects, syncManage, syncQuotaDialog } from "../pages/manage.js";
import { newItem, newRepo } from "../pages/vcs.js";
import { groupManage, userManage } from "../pages/people.js";
import { accountUrl, deviceRename, pairDialog, passwordProof, profileConfirm, vcsTokenDialog } from "../pages/account.js";
import { browseUrl, preview } from "../pages/files.js";
import { conflictsDialog, folderForm, folderManage, historyDialog, rollbackDialog, rollbackPreview } from "../pages/sync.js";
import { panelAction, panelDialog } from "../pages/panel.js";
import { previewPackage } from "./package.js";
import { cropSquare } from "./crop.js";
import { forget, installDialog, registry, search } from "../pages/marketplace.js";

let busy = false;
// Profile changes that wait for the password dialog.
let pendingProfile = null;

export function formData(form) {
  const values = Object.fromEntries(new FormData(form));
  for (const input of form.querySelectorAll('input[type="checkbox"]')) values[input.name] = input.checked;
  // Size boxes show MB or GB; the server always takes MiB.
  for (const input of form.querySelectorAll("input[data-size]")) {
    const unit = Number(input.parentElement.querySelector("[data-size-unit]").value);
    values[input.name] = input.value === "" ? "" : String(Math.round(Number(input.value) * unit));
  }
  return values;
}

/** The server's current settings with a form's changes applied over them. */
function settingsWith(data) {
  const current = store.state.settings, startup = store.state.startup;
  const merged = { ...current, startup: current.startup && !startup?.needs_confirmation, ...data };
  for (const key of ["sync_quota_mb", "sync_max_file_mb", ...Object.keys(vcsLimits)]) merged[key] = Number(merged[key]);
  return merged;
}

const secret = (title, text, value) => modal(title, html`<p>${text}</p><pre class="secret">${value}</pre>`);

export async function onSubmit(event) {
  const form = event.target.closest("form[data-form]");
  if (!form) return;
  event.preventDefault();
  if (busy) return;
  busy = true;
  const submit = form.querySelector('button[type="submit"]');
  if (submit) { submit.disabled = true; submit.setAttribute("aria-busy", "true"); }
  try {
    const data = formData(form), kind = form.dataset.form, id = form.dataset.id || "";
    if (kind === "setup") { await submitSetup(data); return; }
    if (["login", "register", "reset"].includes(kind)) {
      const response = await api(`/api/${kind}`, data);
      if (kind === "register") {
        toast(response.status === "pending" ? "Account requested. The owner will review it." : "Account created. You can sign in.");
        location.href = "/login";
      } else if (kind === "login") {
        const next = new URLSearchParams(location.search).get("next");
        // Signing in from a device pairing link returns to it, code included.
        location.href = next?.startsWith("/oidc/authorize?") ? next : location.pathname === "/pair" ? location.pathname + location.search : "/";
      } else location.href = "/login";
      return;
    }
    let session = false;
    if (kind === "import") { await api("/api/apps/import", new FormData(form)); toast("Application imported."); close(); }
    else if (kind === "backup-import") { await api("/api/backups/import", new FormData(form)); close(); toast("Backup imported. Use Restore a backup to review and restore it."); }
    else if (kind === "profile") {
      // The bio saves on its own; name, username and email need the password first.
      const u = store.session.user;
      const identity = { username: data.username, display_name: data.display_name, email: data.email };
      const identityChanged = Object.keys(identity).some(key => (identity[key] ?? "") !== (u[key] ?? ""));
      const bio = (data.bio ?? "") !== (u.bio ?? "") ? data.bio : null;
      if (!identityChanged && bio === null) { toast("Nothing has changed."); return; }
      if (identityChanged) { pendingProfile = { identity, bio }; profileConfirm(); return; }
      await act("bio", "", { bio });
      session = true;
      toast("Profile saved.");
    }
    else if (kind === "profile-confirm") {
      if (!pendingProfile) { close(); return; }
      const { identity, bio } = pendingProfile;
      await act("profile", "", { ...identity, ...data });
      if (bio !== null) await act("bio", "", { bio });
      pendingProfile = null;
      close();
      session = true;
      toast("Profile saved.");
    }
    else if (["password", "mfa-start", "mfa-disable"].includes(kind)) {
      if (kind === "password") {
        if (data.password !== data.password_repeat) throw new Error("The new passwords don’t match.");
        delete data.password_repeat;
      }
      const result = await act(kind, "", data);
      if (kind === "password") { location.href = "/login"; return; }
      if (kind === "mfa-start") {
        modal("Connect your authenticator", html`<p>Add a time-based account in your authenticator app using this secret.</p><pre class="secret">${result.secret}</pre><p class="help mono">${result.uri}</p>${Form({ name: "mfa-enable", body: html`${Field({ name: "code", label: "Current six-digit code", required: true, autocomplete: "one-time-code", inputmode: "numeric" })}${FormFooter(Submit({ label: "Turn on two-factor authentication" }))}` })}`);
        return;
      }
      close();
      session = true;
      toast("Account updated.");
    }
    else if (kind === "mfa-enable") {
      const result = await act(kind, "", data);
      modal("Save your recovery codes", html`<p>Each code works once. Store them somewhere private before closing this window.</p><pre class="secret">${result.recovery_codes.join("\n")}</pre>`);
      await refresh();
      return;
    }
    else if (kind === "storage") {
      await act("storage", "", data);
      close();
      toast("LOKI is restarting with the selected data. Reconnect shortly.");
      setTimeout(() => location.href = "/login", 6000);
      return;
    }
    else if (kind === "settings") {
      // Settings are edited a few at a time, so each form sends only its own fields.
      const result = await submitStartupAware(confirmation => act("settings", "", { ...settingsWith(data), startup_confirmation: confirmation }));
      if (!result) return;
      if (form.closest("dialog")) close();
      session = true;
      toast("Settings saved.");
    }
    else if (kind === "share") { await act("share-save", id, data); close(); toast("File share saved. Configure access to share it with others."); }
    else if (kind === "route") { await act("route-save", id, data); close(); toast("Route saved. Enable it when ready."); }
    else if (kind === "access") {
      const grants = [...form.querySelectorAll(".grant-row")].map(row => {
        const value = $(".grant-subject", row).value, at = value.indexOf(":");
        return { subject_kind: value.slice(0, at), subject: value.slice(at + 1), role: $(".grant-role", row).value };
      });
      await act("resource-access", id, { visibility: data.visibility, grants });
      close();
      toast("Access updated.");
    }
    else if (kind === "app-config") {
      const r = resource(id), config = {};
      for (const f of r.manifest.fields) config[f.key] = f.kind === "number" ? Number(data[f.key]) : data[f.key];
      const storage_mib = data.storage_mib ? Number(data.storage_mib) : null;
      await act("app-config", id, { hostname: data.hostname, lan_bind: data.lan_bind ?? "", config, storage_mib });
      close();
      toast("App settings saved.");
    }
    else if (kind === "user-create") {
      const result = await act("user-create", "", data);
      secret("Account activation link", "Give this single-use link to the new member. It expires in 24 hours.", result.url);
      await refresh();
      return;
    }
    else if (kind === "group-create") { await act("group-create", "", data); close(); toast("Group created."); }
    else if (kind === "sync-folder") {
      if (id) data.keep_versions = Number(data.keep_versions);
      await act("sync-folder-save", id, data);
      close();
      toast(id ? "Folder saved." : "Folder created. Map it to a local directory on each device.");
    }
    else if (kind === "sync-folder-delete") {
      await act(kind, id, data);
      close();
      toast("Folder deleted.");
      await refresh();
      await navigate("/sync", { replace: true });
      return;
    }
    else if (kind === "sync-rollback-time") {
      const time = Math.floor(new Date(data.when).getTime() / 1000);
      if (!Number.isFinite(time)) return toast("Choose a date and time.", true);
      if (time >= Date.now() / 1000) return toast("Choose a time in the past.", true);
      return await rollbackPreview(id, time);
    }
    else if (kind === "sync-rollback") {
      const result = await act(kind, id, { time: Number(form.dataset.time), remove_newer: Boolean(data.remove_newer) });
      close();
      const done = `Rolled back: ${result.restored} brought back, ${result.removed} removed. Devices pick it up on their next sync.`;
      toast(result.skipped.length ? `${done} ${result.skipped.length} skipped, such as ${result.skipped[0].path}: ${result.skipped[0].error}` : done, result.skipped.length > 0);
    }
    else if (kind === "device-rename" || kind === "sync-device-rename") { await act(kind, id, data); close(); toast("Device renamed."); }
    else if (kind === "vcs-quota") {
      const key = id || data.subject, size = value => value === "" || value === undefined ? null : Number(value);
      if (!key) return toast("Choose a person or group first.", true);
      const [subject, ...rest] = key.split(":"), target = rest.join(":");
      await act(subject === "group" ? "vcs-group-quota" : "vcs-quota", target, { repo_mib: size(data.repo_mib), total_mib: size(data.total_mib) });
      await refresh();
      toast("Repository limits saved.");
      return vcsManage("storage");
    }
    else if (kind === "sync-quota") {
      const user = id || data.user;
      if (!user) return toast("Choose a person first.", true);
      await act(kind, user, { mb: data.mb === "" ? null : Number(data.mb) });
      await refresh();
      toast("Save sync storage saved.");
      return syncManage(undefined, "storage");
    }
    else if (kind === "personal-enable") { await api(`/api/personal/settings/apps/${enc(id)}`, { enabled: data.enabled }); close(); toast("Personal storage permission saved."); }
    else if (kind === "vcs-create") {
      const repo = await api("/api/repos", { name: data.name, upstream: id || null, default_branch: data.default_branch || null, group: data.group || null });
      close();
      return await navigate(`/repositories/${enc(repo.id)}`);
    }
    else if (kind === "vcs-transfer") {
      const [targetKind, ...rest] = data.target.split(":"), target = rest.join(":");
      const label = form.querySelector("select[name=target] option:checked")?.textContent || "the new owner";
      if (!(await ask(["Transfer this repository?", `Ownership moves to ${label}. If they need to accept, pushes and merges pause until they decide. You may lose access you only had as the owner.`, "Transfer"]))) return;
      const result = await api(`/api/repos/${enc(id)}/transfer`, { action: "start", kind: targetKind, target });
      toast(result.transferred ? `Transferred to ${result.to}.` : `Transfer requested. ${result.to} needs to accept it; changes are paused until then.`);
    }
    else if (kind === "vcs-grant") {
      const [subject_kind, subject] = data.subject.split(":");
      await api(`/api/repos/${enc(id)}/permissions`, { subject_kind, subject, role: data.role });
      toast("Access updated.");
    }
    else if (kind === "vcs-token") {
      const { scope, ...input } = data;
      const result = await api("/api/account/vcs-tokens", { ...input, write: scope === "write" });
      secret("Skald token", "Copy the token now and set SKALD_TOKEN before running skald.", result.token);
      return await refresh();
    }
    else if (kind === "vcs-settings") {
      await api(`/api/repos/${enc(id)}/settings`, data);
      toast("Repository settings saved.");
    }
    else if (kind === "vcs-item") {
      const item = await api(`/api/repos/${enc(id)}/items`, data);
      close();
      return await navigate(`/repositories/${enc(id)}/items/${enc(item.id)}`);
    }
    else if (kind === "vcs-comment") {
      await api(`/api/repos/${enc(id)}/items/${enc(form.dataset.item)}`, { action: "comment", body: data.body });
      form.reset();
    }
    else if (kind === "device-owner-approve") {
      await act("device-approve", "", { code: form.dataset.code, proof: data });
      close();
      toast("Owner API access approved.");
      return await navigate(accountUrl("devices"));
    }
    else if (kind === "device-code") { pairDialog(await act("device-lookup", "", data)); return; }
    else if (kind === "panel-upload") {
      const body = new FormData(form);
      body.set("action", form.dataset.name);
      await panelAction(id, body);
      return;
    }
    else if (kind === "panel-form") {
      const fields = Object.fromEntries(Object.entries(data).map(([k, v]) => [k, String(v)]));
      await panelAction(id, { action: form.dataset.name, value: "", fields });
      return;
    }
    await refresh({ session });
  } catch (error) {
    toast(error.message, true);
  } finally {
    busy = false;
    if (submit) { submit.disabled = false; submit.removeAttribute("aria-busy"); }
  }
}

// Buttons that only open a dialog; nothing changes on the server.
const dialogs = {
  "backup-import": () => modal("Import a backup", html`<p>Choose a LOKI backup ZIP up to 2 GiB. Importing stores it here; restoring is a separate step.</p>${Notice({ tone: "warn", body: "Backups include private credentials and native app packages, so use a source you trust." })}${Form({ name: "backup-import", body: html`${FileInput({ name: "backup", label: "Backup archive", accept: ".zip" })}${FormFooter(Submit({ label: "Import backup", icon: "upload" }))}` })}`),
  "app-import": () => modal("Import a LOKI application", html`${Notice({ tone: "info", body: "Apps are WASI components: they share LOKI’s web server and can use only the app services LOKI gives them. Review the package source before importing." })}${Form({ name: "import", body: html`${FileInput({ name: "package", label: "Application package (.loki-app)", accept: ".loki-app", help: "Up to 512 MiB. Replacing an installed package snapshots its stopped data before updating, including builds with the same prerelease version." })}<div id="package-preview" aria-live="polite"></div>${FormFooter(Button({ label: "Import package", type: "submit", kind: "primary", disabled: true }))}` })}`),
  "app-manage": id => appManage(id),
  "share-manage": id => shareManage(id),
  "route-manage": id => routeManage(id),
  "share-new": () => modal("Share a folder", shareForm()),
  "route-new": () => modal("Connect a service", routeForm()),
  "user-new": () => modal("Create an account", Form({ name: "user-create", body: html`${Field({ name: "username", label: "Username", required: true, minlength: 3, maxlength: 32, help: "The member chooses their own password using a one-time activation link." })}${FormFooter(Submit({ label: "Create account" }))}` })),
  "user-manage": id => userManage(id),
  "group-new": () => modal("Create a group", Form({ name: "group-create", body: html`${Field({ name: "name", label: "Group name", required: true, maxlength: 80 })}${FormFooter(Submit({ label: "Create group" }))}` })),
  "mfa-start": () => modal("Set up two-factor authentication", Form({ name: "mfa-start", body: html`${passwordProof()}${FormFooter(Submit({ label: "Continue" }))}` })),
  "mfa-disable": () => modal("Turn off two-factor authentication", Form({ name: "mfa-disable", body: html`${passwordProof()}${FormFooter(Submit({ label: "Turn off", kind: "danger" }))}` })),
  "sync-manage": id => syncManage(id),
  "sync-pair": () => navigate("/pair"),
  "device-pair": () => navigate("/pair"),
  "device-rename": id => deviceRename(id),
  "sync-folder-new": () => modal("New sync folder", folderForm()),
  "sync-folder-manage": id => folderManage(id),
  "sync-device-rename": id => deviceRename(id),
  "sync-conflicts": id => conflictsDialog(id),
  "sync-rollback-open": id => rollbackDialog(id),
  "storage-move": () => storage("Move server data", false),
  "storage-restore": () => storage("Restore a backup", true),
};
function storage(title, restore) {
  modal(title, html`<p>Choose an empty directory on this server. The current installation is kept, and LOKI restarts using the selected data. Shared folders outside LOKI stay where they are.</p>${Form({ name: "storage", body: html`${Field({ name: "target", label: "New empty data directory", required: true })}${restore ? Select({ name: "backup", label: "Backup to restore", options: store.state.backups, attrs: { required: true } }) : ""}${passwordProof()}${FormFooter(Submit({ label: "Prepare data & restart LOKI" }))}` })}`);
}

const warnings = {
  uninstall: ["Uninstall this application?", "App data and shared files will be kept.", "Uninstall"],
  delete: ["Delete this route?", "The service itself keeps running; LOKI stops forwarding to it.", "Delete route"],
  rollback: ["Roll back this application?", "Restore the previous package and its data snapshot? Changes since the snapshot will be replaced.", "Roll back"],
};
const ask = ([title, message, confirm]) => confirmDialog({ title, message, confirm });

let folderReturn = null;

export async function onClick(event) {
  // Dropdown menus close when something outside them, or a link inside them, is clicked.
  for (const open of document.querySelectorAll?.("details.menu[open]") ?? []) {
    if (!open.contains(event.target) || event.target.closest(".menu-panel a")) open.removeAttribute("open");
  }
  const previewLink = event.target.closest("[data-preview]");
  if (previewLink && !event.ctrlKey && !event.metaKey) {
    event.preventDefault();
    preview(previewLink.dataset.share, previewLink.dataset.preview);
    return;
  }
  const link = event.target.closest("a[href]");
  if (link && routable(link, event)) {
    event.preventDefault();
    if ($("#dialog").open) close();
    navigate(link.href).catch(error => toast(error.message, true));
    return;
  }
  const b = event.target.closest("[data-action]");
  if (!b) return;
  event.preventDefault();
  const id = b.dataset.id, action = b.dataset.action;
  try {
    if (dialogs[action]) return dialogs[action](id);
    if (action === "vcs-manage") return vcsManage();
    if (action === "vcs-quota-edit") return vcsQuotaDialog(id);
    if (action === "sync-quota-edit") return syncQuotaDialog(id);
    if (action === "storage-refresh") {
      b.setAttribute("aria-busy", "true");
      await api("/api/storage-usage?refresh=true");
      return await refresh();
    }
    if (action === "sync-quota-clear") {
      await act("sync-quota", id, { mb: null });
      await refresh();
      toast("Storage limit removed. They use the default again.");
      return syncManage(undefined, "storage");
    }
    if (action === "vcs-quota-clear") {
      const [kind, ...rest] = id.split(":");
      await act(kind === "group" ? "vcs-group-quota" : "vcs-quota", rest.join(":"), { repo_mib: null, total_mib: null });
      await refresh();
      toast("Limits removed. They use the defaults again.");
      return vcsManage("storage");
    }
    if (action === "panel-tab") {
      const tabs = b.closest(".panel-tabs");
      for (const tab of tabs.querySelectorAll(":scope > .tabs > [role=tab]")) {
        const active = tab === b;
        tab.classList.toggle("active", active);
        tab.setAttribute("aria-selected", String(active));
      }
      for (const panel of tabs.querySelectorAll(":scope > [role=tabpanel]")) panel.hidden = panel.dataset.panel !== id;
      return;
    }
    if (action === "vcs-new-item") return await newItem(id, b.dataset.kind);
    if (action === "vcs-new") return await newRepo();
    if (action === "vcs-fork") return await newRepo(id);
    if (action === "vcs-transfer-decide") {
      const change = b.dataset.change;
      if (change === "decline" && !(await ask(["Decline this transfer?", "The repository stays with its current owner, and code changes resume.", "Decline"]))) return;
      await api(`/api/repos/${enc(id)}/transfer`, { action: change });
      toast({ accept: "Transfer accepted. The repository is now yours to manage.", decline: "Transfer declined.", cancel: "Transfer cancelled." }[change] || "Updated.");
      return await refresh();
    }
    if (action === "group-manage") return groupManage(id);
    if (action === "resource-icon-remove") {
      await api(`/api/resources/${enc(id)}/icon`, {}, "DELETE");
      store.avatarVersion = Date.now();
      await refresh();
      toast("Icon removed.");
      return resource(id)?.kind === "share" ? shareManage(id) : routeManage(id);
    }
    if (action === "group-icon-remove") {
      await api(`/api/groups/${enc(id)}/icon`, {}, "DELETE");
      store.avatarVersion = Date.now();
      await refresh();
      return groupManage(id);
    }
    if (action === "vcs-delete") {
      if (!(await ask(["Delete this repository?", `This removes ${b.dataset.name}, including its revisions, files, and discussions.`, "Delete repository"]))) return;
      await api(`/api/repos/${enc(id)}`, {}, "DELETE");
      return await navigate("/repositories");
    }
    if (action === "vcs-new-token") return vcsTokenDialog();
    if (action === "copy") {
      await navigator.clipboard.writeText(b.dataset.copy);
      return toast("Copied to the clipboard.");
    }
    if (action === "vcs-remove-access") {
      await api(`/api/repos/${enc(id)}/permissions`, { subject_kind: b.dataset.kind, subject: b.dataset.subject, role: "remove" });
      return await refresh();
    }
    if (action === "vcs-revoke-token") {
      if (!(await ask(["Revoke this Skald token?", "Clients using this token will need a new one.", "Revoke token"]))) return;
      await api(`/api/account/vcs-tokens/${enc(id)}`, {}, "DELETE");
      return await refresh();
    }
    if (action === "vcs-verify" || action === "vcs-gc") {
      const result = await api(`/api/repos/${enc(id)}/maintenance`, { operation: action === "vcs-verify" ? "verify" : "gc" });
      return toast(action === "vcs-verify" ? "Repository verified." : `Removed ${result.removed} unreachable objects.`);
    }
    if (action === "vcs-item-action") {
      await api(`/api/repos/${enc(id)}/items/${enc(b.dataset.item)}`, { action: b.dataset.change, head: b.dataset.head || null, base: b.dataset.base || null });
      return await refresh();
    }
    if (action === "app-panel") return await panelDialog(id);
    if (action === "registry-install") return await installDialog(id);
    if (action === "registry-refresh") {
      b.disabled = true;
      try { await registry(true); } finally { b.disabled = false; }
      return await refresh();
    }
    if (action === "registry-confirm") {
      const label = b.innerHTML;
      b.disabled = true;
      b.textContent = "Downloading and installing…";
      let result;
      try { result = await api("/api/registry/install", { id }); }
      catch (error) { b.innerHTML = label; throw error; }
      finally { b.disabled = false; }
      close();
      forget();
      toast(`${result.manifest.name} ${result.manifest.version} installed.`);
      return await refresh();
    }
    if (action === "panel-action") {
      if (b.dataset.confirm && !(await ask([b.dataset.confirm, "", b.dataset.label]))) return;
      return await panelAction(id, { action: b.dataset.name, value: b.dataset.value, fields: {} });
    }
    if (action === "reload") return location.reload();
    if (action === "grant-remove") { b.closest(".grant-row").remove(); return; }
    if (action === "grant-add") { $("#grants").insertAdjacentHTML("beforeend", String(grantRow(subjects(), roles(resource(id))))); return; }
    if (action === "folder-picker") {
      const form = b.closest("form");
      folderReturn = { id: form.dataset.id || "", name: form.elements.name.value, excluded_extensions: form.elements.excluded_extensions.value, supports_updates: form.elements.supports_updates.checked };
      return await folderPicker(form.elements.root.value);
    }
    if (action === "folder-open") return await folderPicker(b.dataset.path);
    if (action === "folder-select") {
      const draft = { ...folderReturn, root: b.dataset.path };
      if (draft.id) shareManage(draft.id, draft);
      else modal("Share a folder", shareForm(draft));
      return;
    }
    if (action === "sync-history") return await historyDialog(id, b.dataset.path);
    if (action === "sync-rollback-preview") return await rollbackPreview(id, Number(b.dataset.time));
    if (action === "logs") {
      const result = await api(`/api/logs/${enc(id)}`);
      return modal(id === "caddy" ? "Caddy logs" : "Service logs", html`<pre class="log">${result.text || "No log output yet."}</pre>`);
    }
    if (action === "caddy-preview") {
      const result = await act(action, "", settingsWith(formData(b.closest("form"))));
      return modal("Validated Caddy configuration", jsonView(result));
    }
    if (action === "caddy-adopt") {
      const result = await act(action), form = b.closest("form");
      form.elements.caddy_base.value = result.base;
      form.elements.caddy_mode.value = "existing";
      form.querySelector("details.disclosure")?.setAttribute("open", "");
      toast("Existing configuration backed up and loaded into the editor. Review and save to adopt it.");
      return;
    }
    if ((action === "app-operation" || action === "route-operation") && warnings[b.dataset.operation] && !(await ask(warnings[b.dataset.operation]))) return;
    if (action === "group-delete" && !(await ask(["Delete this group?", "The group and its resource permissions are removed. Its members keep their accounts.", "Delete group"]))) return;
    if (action === "share-delete" && !(await ask(["Remove this share?", "Files on disk are kept. People lose access through LOKI.", "Remove share"]))) return;
    if (action === "revoke-all" && !(await ask(["Sign out everywhere?", "Every browser and application session for your account ends, including this one.", "Sign out everywhere"]))) return;
    if (action === "sync-device-revoke" && !(await ask(["Sign out this device?", "It stops syncing until you pair it again. Files already on it stay there.", "Sign out device"]))) return;
    b.disabled = true;
    b.setAttribute("aria-busy", "true");
    if (action === "logout") { await api("/api/logout", {}); location.href = "/login"; return; }
    if (action === "revoke-all") { await act(action); location.href = "/login"; return; }
    if (action === "app-operation" || action === "route-operation") {
      await act(action === "app-operation" ? "app-action" : "route-action", id, { operation: b.dataset.operation });
      close();
    }
    else if (action === "service-operation") {
      const result = await act("service", id, { operation: b.dataset.operation });
      return modal("Service result", html`<pre class="log">${result.message}</pre>`);
    }
    else if (action === "user-status") { await act(action, id, { status: b.dataset.status }); close(); }
    else if (action === "reset-issue") {
      const result = await act(action, id);
      return secret("One-time password reset link", "Give this link to the member. It expires in 24 hours. Existing sessions are revoked when the new password is set; two-factor authentication remains enabled.", result.url);
    }
    else if (action === "group-delete") { await act(action, id); close(); }
    else if (action === "share-delete") { await act("resource-delete", id); close(); }
    else if (action === "session-revoke") await act(action, id);
    else if (action === "device-revoke" || action === "sync-device-revoke") await act(action, id);
    else if (action === "device-approve" || action === "device-deny") {
      await act(action, "", { code: b.dataset.code });
      close();
      toast(action === "device-approve" ? "Device approved. It will finish signing in by itself." : "Sign-in declined.");
      await refresh();
      return await navigate(accountUrl("devices"));
    }
    else if (action === "sync-restore" || action === "sync-conflict-resolve") {
      await act(action, id, { history_id: Number(b.dataset.history), keep: b.dataset.keep });
      close();
    }
    else if (action === "diagnostics") return modal("Server diagnostics", jsonView(await act(action)));
    else if (action === "backup") {
      const result = await act(action);
      toast("Backup created.");
      await refresh();
      return modal("Backup ready", html`<p>Download and store this snapshot somewhere private.</p>${FormFooter(LinkButton({ label: "Download backup", href: `/api/backups/${enc(result.file)}`, download: true, kind: "primary", icon: "download" }), { divider: false })}`);
    }
    else if (action === "caddy-apply") await act(action);
    toast("Updated.");
    await refresh();
  } catch (error) {
    toast(error.message, true);
  } finally {
    b.disabled = false;
    b.removeAttribute?.("aria-busy");
  }
}

/** Sends a cropped picture, showing that it is on its way. */
async function uploadPicture(path, square) {
  const save = $('#dialog [data-crop="save"]');
  save?.setAttribute("aria-busy", "true");
  const body = new FormData();
  body.append("file", square, "picture.png");
  await api(path, body);
  store.avatarVersion = Date.now();
}

/** Keeps a "characters left" count beside text boxes that have one. */
export function onInput(event) {
  const e = event.target;
  if (e.matches?.("[data-registry-search]")) return search(e);
  if (!e.dataset?.count) return;
  const counter = document.getElementById(e.dataset.count);
  if (counter) counter.textContent = Number(e.maxLength) - [...e.value].length;
}

export async function onChange(event) {
  const e = event.target;
  // Switching a size between MB and GB converts what is typed, rounding to whole gigabytes.
  if (e.matches("select[data-size-unit]")) {
    const input = e.parentElement.querySelector("input[data-size]");
    const from = Number(e.dataset.sizeUnit), to = Number(e.value);
    if (input.value !== "") {
      const mib = Number(input.value) * from;
      input.value = to > from ? Math.max(1, Math.round(mib / to)) : Math.round(mib / to);
    }
    input.max = Math.floor(Number(input.dataset.maxMib) / to);
    e.dataset.sizeUnit = to;
    return;
  }
  // A group icon, like a profile picture, is cropped to a square and uploads at once.
  if (e.matches("input[data-group-icon]") && e.files?.length) {
    const group = e.dataset.groupIcon, file = e.files[0];
    e.value = "";
    try {
      const square = await cropSquare(file, { title: "Crop the group icon" });
      if (square) {
        await uploadPicture(`/api/groups/${enc(group)}/icon`, square);
        await refresh();
        toast("Group icon updated.");
      }
    } catch (error) {
      toast(error.message, true);
    }
    groupManage(group);
    return;
  }
  // A connected service's or file share's icon, cropped and uploaded the same way.
  if (e.matches("input[data-resource-icon]") && e.files?.length) {
    const id = e.dataset.resourceIcon, file = e.files[0], kind = resource(id)?.kind;
    e.value = "";
    try {
      const square = await cropSquare(file, { title: kind === "share" ? "Crop the share's icon" : "Crop the service's icon" });
      if (square) {
        await uploadPicture(`/api/resources/${enc(id)}/icon`, square);
        await refresh();
        toast("Icon updated.");
      }
    } catch (error) {
      toast(error.message, true);
    }
    return kind === "share" ? shareManage(id) : routeManage(id);
  }
  // A new profile picture is cropped to a square, then uploads at once.
  if (e.matches("input[data-avatar]") && e.files?.length) {
    const file = e.files[0];
    e.value = "";
    try {
      const square = await cropSquare(file);
      if (square) {
        await uploadPicture("/api/avatar", square);
        close();
        await refresh({ session: true });
        toast("Profile picture updated.");
      }
    } catch (error) {
      if ($("#dialog").open) close();
      toast(error.message, true);
    }
    return;
  }
  if (e.matches("[data-membership]")) {
    try {
      await act("membership", e.dataset.membership, { group: e.dataset.group, enabled: e.checked });
      await refresh();
      toast("Group membership updated.");
    } catch (error) {
      e.checked = !e.checked;
      toast(error.message, true);
    }
  }
  if (e.matches('form[data-form="import"] input[type="file"]')) await previewPackage(e);
  if (e.id === "file-sort") navigate(browseUrl(e.dataset.share, e.dataset.path, 0, e.value)).catch(error => toast(error.message, true));
}

