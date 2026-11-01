// Reads a .loki-app package in the browser so the import dialog can say what it is
// before anything is uploaded. Only the zip directory and manifest.json are read.
// The server still checks every file and checksum when it installs the package.
import { $, esc, fmt, resource, store } from "./core.js";
import { tag } from "./ui.js";

const MAX_PACKAGE = 536870912, MAX_MANIFEST = 131072, MAX_FILES = 2048;
const CAPABILITIES = ["database", "uploads", "forms", "storage", "realtime"];
const SEMVER = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/;

const notPackage = () => new Error("This file is not a LOKI app package.");
const bytes = async (file, start, end) => new DataView(await file.slice(start, end).arrayBuffer());

/** The package's manifest and the names of the files in it. */
export async function readPackage(file) {
  const tail = await bytes(file, Math.max(0, file.size - 65557), file.size);
  let end = -1;
  for (let i = tail.byteLength - 22; i >= 0; i--) if (tail.getUint32(i, true) === 0x06054b50) { end = i; break; }
  if (end < 0) throw notPackage();
  const count = tail.getUint16(end + 10, true), size = tail.getUint32(end + 12, true), offset = tail.getUint32(end + 16, true);
  if (count > MAX_FILES) throw new Error("Too many package files.");
  if (offset + size > file.size) throw notPackage();
  const dir = await bytes(file, offset, offset + size), names = new Set(), text = new TextDecoder();
  let entry = null;
  for (let at = 0, i = 0; i < count; i++) {
    if (at + 46 > dir.byteLength || dir.getUint32(at, true) !== 0x02014b50) throw notPackage();
    const nameLength = dir.getUint16(at + 28, true);
    const name = text.decode(new Uint8Array(dir.buffer, at + 46, Math.min(nameLength, dir.byteLength - at - 46)));
    names.add(name);
    if (name === "manifest.json") entry = { method: dir.getUint16(at + 10, true), compressed: dir.getUint32(at + 20, true), length: dir.getUint32(at + 24, true), local: dir.getUint32(at + 42, true) };
    at += 46 + nameLength + dir.getUint16(at + 30, true) + dir.getUint16(at + 32, true);
  }
  if (!entry) throw new Error("The package has no manifest.json.");
  if (entry.length > MAX_MANIFEST) throw new Error("Manifest is too large.");
  const header = await bytes(file, entry.local, entry.local + 30);
  if (header.byteLength < 30 || header.getUint32(0, true) !== 0x04034b50) throw notPackage();
  const start = entry.local + 30 + header.getUint16(26, true) + header.getUint16(28, true);
  const data = file.slice(start, start + entry.compressed);
  if (entry.method !== 0 && entry.method !== 8) throw new Error("The manifest uses an unsupported compression method.");
  const json = entry.method === 8 ? await inflate(data) : await data.text();
  try { return { manifest: JSON.parse(json), names }; } catch { throw new Error("The package manifest is not valid JSON."); }
}

// Stops early so a small entry cannot expand into gigabytes.
async function inflate(blob) {
  const reader = blob.stream().pipeThrough(new DecompressionStream("deflate-raw")).getReader(), parts = [];
  let total = 0;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      if ((total += value.length) > MAX_MANIFEST) { reader.cancel(); throw new Error("Manifest is too large."); }
      parts.push(value);
    }
  } catch (error) {
    throw error.message === "Manifest is too large." ? error : new Error("The package manifest could not be read.");
  }
  return new Blob(parts).text();
}

/** -1, 0 or 1. Build metadata is ignored. */
function compareVersions(a, b) {
  const [, ...x] = SEMVER.exec(a), [, ...y] = SEMVER.exec(b);
  for (let i = 0; i < 3; i++) if (Number(x[i]) !== Number(y[i])) return Number(x[i]) < Number(y[i]) ? -1 : 1;
  if (!x[3] || !y[3]) return x[3] === y[3] ? 0 : x[3] ? -1 : 1;
  const p = x[3].split("."), q = y[3].split(".");
  for (let i = 0; i < Math.min(p.length, q.length); i++) {
    if (p[i] === q[i]) continue;
    const m = /^\d+$/.test(p[i]), n = /^\d+$/.test(q[i]);
    if (m && n) return Number(p[i]) < Number(q[i]) ? -1 : 1;
    if (m !== n) return m ? -1 : 1;
    return p[i] < q[i] ? -1 : 1;
  }
  return Math.sign(p.length - q.length);
}

// The checks from validate_manifest a person can act on, plus the ones that need this server.
function problems(m, names, file, installed) {
  const out = [], runtime = m.runtime ?? "native", files = m.files && typeof m.files === "object" ? Object.keys(m.files) : [];
  if (file.size > MAX_PACKAGE) out.push("The package is larger than 512 MiB.");
  if (m.protocol !== 1) out.push(`This server reads app protocol 1; the package uses ${esc(m.protocol ?? "none")}.`);
  if (typeof m.id !== "string" || !/^[a-z0-9][a-z0-9-]{0,63}$/.test(m.id) || m.id.startsWith("loki-")) out.push("The app ID is missing or invalid.");
  if (typeof m.name !== "string" || !m.name || m.name.length > 100) out.push("The app name is missing or too long.");
  if (typeof m.version !== "string" || !SEMVER.test(m.version)) out.push("The version is not a valid semantic version.");
  if (runtime !== "native" && runtime !== "wasi") out.push(`Unknown runtime “${esc(runtime)}”.`);
  if (runtime === "native" && m.target !== store.state.target) out.push(`This native package is built for ${esc(m.target || "an unknown target")}; this server needs ${esc(store.state.target)}.`);
  if (runtime === "wasi" && m.target !== "wasm32-wasip2") out.push("WASI packages must target wasm32-wasip2.");
  if (runtime === "wasi" && !String(m.entry).endsWith(".wasm")) out.push("The WASI entry must be a .wasm component.");
  if (!files.includes(m.entry)) out.push("The entry file is not listed in the manifest.");
  if (files.some(f => !names.has(f))) out.push("The package is missing a declared file.");
  const unknown = (m.capabilities || []).filter(c => !CAPABILITIES.includes(c));
  if (unknown.length) out.push(`Unknown capabilities: ${unknown.map(esc).join(", ")}.`);
  if (runtime !== "wasi" && ((m.capabilities || []).includes("realtime") || m.panel || m.site === false)) out.push("Realtime, panels, and panel-only apps need a WASI component.");
  if (installed && !out.length && compareVersions(m.version, installed.manifest.version) < 0) out.push(`Version ${esc(m.version)} is older than the installed ${esc(installed.manifest.version)}. Import the same or a newer version.`);
  return out;
}

const row = (label, value) => `<div class="kv-row"><dt>${esc(label)}</dt><dd>${value}</dd></div>`;
const list = items => items.length ? `<div class="tags">${items.map(i => tag(i)).join("")}</div>` : '<span class="kv-none">None</span>';

function summary(m, names, file, installed, issues) {
  const wasi = (m.runtime ?? "native") === "wasi";
  const order = installed && SEMVER.test(m.version) ? compareVersions(m.version, installed.manifest.version) : 1, was = installed?.manifest.version;
  const change = !installed ? tag("New app") : order === 0 ? tag(`Reinstalls v${was}`, "warn") : order < 0 ? tag(`Older than v${was}`, "bad") : tag(`Updates v${was}`);
  return `<div class="package-preview"><h3>${esc(m.name || m.id || "Unnamed app")}</h3><div class="tags">${tag(`v${m.version}`)}${tag(wasi ? "WASI component" : "Native service", wasi ? "" : "warn")}${change}</div>${m.description ? `<p>${esc(m.description)}</p>` : ""}`
    + `<dl class="kv">${row("App ID", `<span class="kv-str">${esc(m.id)}</span>`)}${row("Target", `<span class="kv-str">${esc(m.target)}</span>`)}${row("Capabilities", list(m.capabilities || []))}${row("Roles", list(m.roles || []))}${row("Settings", m.fields?.length ? esc(m.fields.map(f => f.label || f.key).join(", ")) : '<span class="kv-none">None</span>')}${row("Panel", m.panel ? "Yes" : "No")}${row("Package", `${esc(fmt(file.size))}, ${names.size} files`)}</dl>`
    + (!wasi && !issues.length ? '<p class="notice">This native service will run with your server user’s permissions. Only import it if you trust where it came from.</p>' : "")
    + (installed && !issues.length ? '<p class="help">LOKI stops the installed version and snapshots its data before updating.</p>' : "")
    + (issues.length ? `<div class="notice error">LOKI cannot import this package:<ul>${issues.map(i => `<li>${i}</li>`).join("")}</ul></div>` : "")
    + "</div>";
}

let reading = 0;
/** Shows what the chosen package is, and allows importing it only when it looks installable. */
export async function previewPackage(input) {
  const form = input.form, box = $("#package-preview", form), submit = form.querySelector('button:not([type="button"])');
  const file = input.files[0], run = ++reading;
  submit.disabled = true;
  submit.textContent = "Import package";
  if (!file) { box.innerHTML = ""; return; }
  box.innerHTML = '<p class="help">Reading package…</p>';
  let html, label = "";
  try {
    const { manifest, names } = await readPackage(file);
    if (!manifest || typeof manifest !== "object" || Array.isArray(manifest) || ["capabilities", "roles", "fields"].some(k => manifest[k] !== undefined && !Array.isArray(manifest[k]))) throw new Error("The package manifest is not valid.");
    const installed = resource(manifest.id)?.kind === "app" ? resource(manifest.id) : null, issues = problems(manifest, names, file, installed);
    html = summary(manifest, names, file, installed, issues);
    if (!issues.length) label = `${installed ? "Update" : "Install"} ${manifest.name}`;
  } catch (error) {
    html = `<p class="notice error">${esc(error instanceof TypeError ? "The package manifest is not valid." : error.message)}</p>`;
  }
  if (run !== reading) return;
  box.innerHTML = html;
  if (label) { submit.textContent = label; submit.disabled = false; }
}
