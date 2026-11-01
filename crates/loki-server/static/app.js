// LOKI dashboard entry point. Framework-free ES modules; see lib/ and pages/.
import { $, api, close, store } from "./lib/core.js";
import { render, refreshState, showError } from "./lib/router.js";
import { onChange, onClick, onInput, onSubmit } from "./lib/actions.js";

// The server embeds the session (and state, when signed in) so the first paint
// needs no request. Visitors without a session fetch one instead.
function boot() {
  const node = document.getElementById("boot");
  if (!node) return;
  try {
    const data = JSON.parse(node.textContent);
    store.session = data.session;
    if (data.state) { store.state = data.state; store.loadedAt = Date.now(); }
  } catch {}
  node.remove();
}

async function start() {
  boot();
  if (!store.session) store.session = await api("/api/session");
  if (store.session.user && !store.state) await refreshState();
  await render();
}

document.addEventListener("click", onClick);
document.addEventListener("submit", onSubmit);
document.addEventListener("change", onChange);
document.addEventListener("input", onInput);
addEventListener("popstate", () => render({ transition: true }).catch(showError));
$("#dialog-close").addEventListener("click", close);
// The close event arrives asynchronously; another dialog may already be showing.
$("#dialog").addEventListener("close", () => { if (!$("#dialog").open) $("#dialog-body").innerHTML = ""; });
$("#toast").addEventListener("click", event => { event.currentTarget.hidden = true; });
start().catch(showError);
