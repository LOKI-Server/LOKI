// What happens when people submit forms, press buttons, and change controls.
import { $, act, api, close, confirmDialog, enc, esc, modal, resource, store, toast } from "./core.js";
import { field, jsonView } from "./ui.js";
import { navigate, refresh, routable } from "./router.js";
import { submitSetup, submitStartupAware } from "../pages/auth.js";
import { appManage, folderPicker, grantRow, roles, routeForm, routeManage, shareForm, shareManage, subjects, syncManage } from "../pages/manage.js";
import { userManage } from "../pages/people.js";
import { passwordProof } from "../pages/account.js";
import { browseUrl, preview } from "../pages/files.js";
import { conflictsDialog, deviceRename, folderForm, folderManage, historyDialog, pairDialog } from "../pages/sync.js";
import { panelAction, panelDialog } from "../pages/panel.js";

let busy = false;

export function formData(form) {
  const values = Object.fromEntries(new FormData(form));
  for (const input of form.querySelectorAll('input[type="checkbox"]')) values[input.name] = input.checked;
  return values;
}

const secret = (title, text, value) => modal(title, `<p>${esc(text)}</p><pre class="secret">${esc(value)}</pre>`);

export async function onSubmit(event) {
  const form = event.target.closest("form[data-form]");
  if (!form) return;
  event.preventDefault();
  if (busy) return;
  busy = true;
  const submit = form.querySelector('button:not([type="button"])');
  if (submit) submit.disabled = true;
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
    else if (kind === "avatar") { await api("/api/avatar", new FormData(form)); store.avatarVersion = Date.now(); form.reset(); session = true; toast("Profile picture updated."); }
    else if (kind === "bio") { await act("bio", "", data); session = true; toast("Bio saved."); }
    else if (["profile", "password", "mfa-start", "mfa-disable"].includes(kind)) {
      const result = await act(kind, "", data);
      if (kind === "password") { location.href = "/login"; return; }
      if (kind === "mfa-start") {
        modal("Connect your authenticator", `<p>Add a time-based account in your authenticator app using this secret.</p><pre class="secret">${esc(result.secret)}</pre><small class="mono">${esc(result.uri)}</small><form data-form="mfa-enable">${field("code", "Current six-digit code", "", "text", "required autocomplete=one-time-code inputmode=numeric")}<div class="actions"><button>Enable two-factor authentication</button></div></form>`);
        return;
      }
      close();
      session = true;
      toast("Account updated.");
    }
    else if (kind === "mfa-enable") {
      const result = await act(kind, "", data);
      modal("Save your recovery codes", `<p>Each code works once. Store them somewhere private before closing this window.</p><pre class="secret">${result.recovery_codes.map(esc).join("\n")}</pre>`);
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
      const numbers = { sync_quota_mb: Number(data.sync_quota_mb), sync_max_file_mb: Number(data.sync_max_file_mb) };
      const result = await submitStartupAware(confirmation => act("settings", "", { ...data, ...numbers, startup_confirmation: confirmation }));
      if (!result) return;
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
      await act("app-config", id, { hostname: data.hostname, lan_bind: data.lan_bind ?? "", config });
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
    else if (kind === "sync-device-rename") { await act(kind, id, data); close(); toast("Device renamed."); }
    else if (kind === "sync-quota") { await act(kind, id, { mb: data.mb === "" ? null : Number(data.mb) }); toast("Quota saved."); }
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
    if (submit) submit.disabled = false;
  }
}

// Buttons that only open a dialog; nothing changes on the server.
const dialogs = {
  "backup-import": () => modal("Import a backup", '<p>Choose a LOKI backup ZIP up to 2 GiB. Importing stores it locally; restoration is a separate action. Backups include private credentials and native app packages, so use a source you trust.</p><form data-form="backup-import"><input type="file" name="backup" aria-label="Backup archive" accept=".zip" required><div class="actions"><button>Import backup archive</button></div></form>'),
  "app-import": () => modal("Import a LOKI application", `<p>Select a portable WASI .loki-app package, or a native package for <strong>${esc(store.state.target)}</strong>.</p><div class="notice">WASI components share LOKI’s web server and use limited app services. Native packages run with your server user’s permissions. Review the package source before importing.</div><form data-form="import"><input type="file" name="package" aria-label="Application package" accept=".loki-app" required><p class="help">Up to 512 MiB. Replacing an installed package snapshots its stopped data before updating, including builds with the same prerelease version.</p><div class="actions"><button>Import package</button></div></form>`),
  "app-manage": id => appManage(id),
  "share-manage": id => shareManage(id),
  "route-manage": id => routeManage(id),
  "share-new": () => modal("Share a folder", shareForm()),
  "route-new": () => modal("Connect a service", routeForm()),
  "user-new": () => modal("Create an account", `<form data-form="user-create">${field("username", "Username", "", "text", "required minlength=3 maxlength=32")}<p class="help">The member chooses their own password using a one-time activation link.</p><div class="actions"><button>Create account</button></div></form>`),
  "user-manage": id => userManage(id),
  "group-new": () => modal("Create a group", `<form data-form="group-create">${field("name", "Group name", "", "text", "required maxlength=80")}<div class="actions"><button>Create group</button></div></form>`),
  "mfa-start": () => modal("Set up two-factor authentication", `<form data-form="mfa-start">${passwordProof()}<div class="actions"><button>Continue</button></div></form>`),
  "mfa-disable": () => modal("Disable two-factor authentication", `<form data-form="mfa-disable">${passwordProof()}<div class="actions"><button>Continue</button></div></form>`),
  "sync-manage": id => syncManage(id),
  "sync-pair": () => navigate("/pair"),
  "sync-folder-new": () => modal("New sync folder", folderForm()),
  "sync-folder-manage": id => folderManage(id),
  "sync-device-rename": id => deviceRename(id),
  "sync-conflicts": id => conflictsDialog(id),
  "storage-move": () => storage("Move server data", false),
  "storage-restore": () => storage("Restore a backup", true),
};
function storage(title, restore) {
  modal(title, `<p>Choose an empty directory on this server. The current installation is retained, and LOKI restarts using the selected data. External shared folders stay in place.</p><form data-form="storage">${field("target", "New empty data directory", "", "text", "required")}${restore ? `<label for="restore-backup">Backup to restore</label><select id="restore-backup" name="backup" required>${store.state.backups.map(b => `<option>${esc(b)}</option>`).join("")}</select>` : ""}${passwordProof()}<div class="actions"><button>Prepare data & restart LOKI</button></div></form>`);
}

const warnings = {
  uninstall: ["Uninstall this application?", "App data and shared files will be kept.", "Uninstall"],
  delete: ["Delete this route?", "The service itself keeps running; LOKI stops forwarding to it.", "Delete route"],
  rollback: ["Roll back this application?", "Restore the previous package and its data snapshot? Changes since the snapshot will be replaced.", "Roll back"],
};
const ask = ([title, message, confirm]) => confirmDialog({ title, message, confirm });

let folderReturn = null;

export async function onClick(event) {
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
    if (action === "app-panel") return await panelDialog(id);
    if (action === "panel-action") {
      if (b.dataset.confirm && !(await ask([b.dataset.confirm, "", b.dataset.label]))) return;
      return await panelAction(id, { action: b.dataset.name, value: b.dataset.value, fields: {} });
    }
    if (action === "reload") return location.reload();
    if (action === "grant-remove") { b.closest(".grant-row").remove(); return; }
    if (action === "grant-add") { $("#grants").insertAdjacentHTML("beforeend", grantRow(subjects(), roles(resource(id)))); return; }
    if (action === "folder-picker") {
      const form = b.closest("form");
      folderReturn = { id: form.dataset.id || "", name: form.elements.name.value, excluded_extensions: form.elements.excluded_extensions.value };
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
    if (action === "logs") {
      const result = await api(`/api/logs/${enc(id)}`);
      return modal(id === "caddy" ? "Caddy logs" : "Service logs", `<pre class="log">${esc(result.text || "No log output yet.")}</pre>`);
    }
    if (action === "caddy-preview") {
      const result = await act(action, "", formData(b.closest("form")));
      return modal("Validated Caddy configuration", jsonView(result));
    }
    if (action === "caddy-adopt") {
      const result = await act(action), form = b.closest("form");
      form.elements.caddy_base.value = result.base;
      form.elements.caddy_mode.value = "existing";
      form.querySelector("details.advanced")?.setAttribute("open", "");
      toast("Existing configuration backed up and loaded into the editor. Review and save to adopt it.");
      return;
    }
    if ((action === "app-operation" || action === "route-operation") && warnings[b.dataset.operation] && !(await ask(warnings[b.dataset.operation]))) return;
    if (action === "group-delete" && !(await ask(["Delete this group?", "The group and its resource permissions are removed. Its members keep their accounts.", "Delete group"]))) return;
    if (action === "share-delete" && !(await ask(["Remove this share?", "Files on disk are kept. People lose access through LOKI.", "Remove share"]))) return;
    if (action === "revoke-all" && !(await ask(["Sign out everywhere?", "Every browser and application session for your account ends, including this one.", "Sign out everywhere"]))) return;
    if (action === "sync-device-revoke" && !(await ask(["Sign out this device?", "It stops syncing until you pair it again. Files already on it stay there.", "Sign out device"]))) return;
    b.disabled = true;
    if (action === "logout") { await api("/api/logout", {}); location.href = "/login"; return; }
    if (action === "revoke-all") { await act(action); location.href = "/login"; return; }
    if (action === "app-operation" || action === "route-operation") {
      await act(action === "app-operation" ? "app-action" : "route-action", id, { operation: b.dataset.operation });
      close();
    }
    else if (action === "service-operation") {
      const result = await act("service", id, { operation: b.dataset.operation });
      return modal("Service result", `<pre class="log">${esc(result.message)}</pre>`);
    }
    else if (action === "user-status") { await act(action, id, { status: b.dataset.status }); close(); }
    else if (action === "reset-issue") {
      const result = await act(action, id);
      return secret("One-time password reset link", "Give this link to the member. It expires in 24 hours. Existing sessions are revoked when the new password is set; two-factor authentication remains enabled.", result.url);
    }
    else if (action === "group-delete") await act(action, id);
    else if (action === "share-delete") { await act("resource-delete", id); close(); }
    else if (action === "session-revoke") await act(action, id);
    else if (action === "sync-device-revoke") await act(action, id);
    else if (action === "device-approve" || action === "device-deny") {
      await act(action, "", { code: b.dataset.code });
      close();
      toast(action === "device-approve" ? "Device approved. It will finish signing in by itself." : "Sign-in declined.");
      await refresh();
      return await navigate(b.dataset.kind === "sync" ? "/sync" : "/");
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
      return modal("Backup ready", `<p>Download and store this snapshot somewhere private.</p><div class="actions"><a class="button" href="/api/backups/${enc(result.file)}" download>Download backup</a></div>`);
    }
    else if (action === "caddy-apply") await act(action);
    toast("Updated.");
    await refresh();
  } catch (error) {
    toast(error.message, true);
  } finally {
    b.disabled = false;
  }
}

/** Keeps a "characters left" count beside text boxes that have one. */
export function onInput(event) {
  const e = event.target;
  if (!e.dataset?.count) return;
  const counter = document.getElementById(e.dataset.count);
  if (counter) counter.textContent = Number(e.maxLength) - [...e.value].length;
}

export async function onChange(event) {
  const e = event.target;
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
  if (e.id === "file-sort") navigate(browseUrl(e.dataset.share, e.dataset.path, 0, e.value)).catch(error => toast(error.message, true));
}

