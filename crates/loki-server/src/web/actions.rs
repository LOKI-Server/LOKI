//! `POST /api/action`: every change the dashboard makes, named by `action`, with
//! the subject in `id` and the action's fields in `data`.
use super::{account, admin};
use crate::{Error, Result, State, auth};
use axum::{Json, extract::State as ExtractState, http::HeaderMap};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

#[derive(Deserialize)]
pub(super) struct Action {
    action: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    data: Value,
}
/// Reads an action's fields. Fields the action doesn't use are ignored.
pub(crate) fn args<T: DeserializeOwned>(data: Value) -> Result<T> {
    let data = if data.is_null() { json!({}) } else { data };
    serde_json::from_value(data).map_err(|e| Error::bad(format!("Invalid request: {e}.")))
}
pub(super) fn ok() -> Value {
    json!({"ok":true})
}

pub(super) async fn action(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
    Json(input): Json<Action>,
) -> Result<Json<Value>> {
    let ctx = auth::context(&state, &headers).await?;
    let user = ctx.signed_in()?;
    let Action { action, id, data } = input;
    let state = &state;
    let output = match action.as_str() {
        "profile" => account::profile(state, ctx.approved()?, args(data)?).await?,
        "bio" => account::bio(state, ctx.approved()?, args(data)?).await?,
        "password" => account::password(state, ctx.approved()?, args(data)?).await?,
        "session-revoke" => account::revoke_session(state, user, &id).await?,
        "revoke-all" => {
            auth::revoke_user(state, &user.id).await?;
            ok()
        }
        "mfa-start" => account::mfa_start(state, ctx.approved()?, args(data)?).await?,
        "mfa-enable" => account::mfa_enable(state, user, args(data)?).await?,
        "mfa-disable" => account::mfa_disable(state, user, args(data)?).await?,
        "device-revoke" => {
            crate::devices::revoke(state, ctx.approved()?, &id).await?;
            ok()
        }
        "device-rename" => {
            #[derive(Deserialize)]
            struct Name {
                name: String,
            }
            let input: Name = args(data)?;
            crate::devices::rename(state, ctx.approved()?, &id, &input.name).await?;
            ok()
        }
        "device-lookup" => {
            let input: account::Code = args(data)?;
            crate::device_codes::lookup(state, ctx.approved()?, &input.code).await?
        }
        "device-approve" | "device-deny" => {
            let input: account::Code = args(data)?;
            let approve = action == "device-approve";
            crate::device_codes::decide(state, ctx.approved()?, &input.code, approve).await?
        }
        name if name.starts_with("sync-") => {
            crate::sync::action(state, ctx.approved()?, name, &id, data).await?
        }
        name => {
            let user = ctx.owner()?;
            match name {
                "user-create" => admin::user_create(state, args(data)?).await?,
                "user-status" => admin::user_status(state, &id, args(data)?).await?,
                "reset-issue" => admin::issue_reset(state, &id, false).await?,
                "group-create" => admin::group_create(state, args(data)?).await?,
                "group-delete" => admin::group_delete(state, &id).await?,
                "membership" => admin::membership(state, &id, args(data)?).await?,
                "resource-access" => admin::resource_access(state, &id, args(data)?).await?,
                "share-save" => admin::share_save(state, &id, args(data)?).await?,
                "resource-delete" => admin::resource_delete(state, &id).await?,
                "route-save" => admin::route_save(state, &id, args(data)?).await?,
                "route-action" => admin::route_action(state, &id, args(data)?).await?,
                "service" => admin::service(state, &id, args(data)?).await?,
                "app-action" => {
                    let input: admin::Operation<String> = args(data)?;
                    crate::apps::action(state, &id, &input.operation).await?;
                    ok()
                }
                "app-config" => admin::app_config(state, &id, args(data)?).await?,
                "settings" => admin::settings(state, args(data)?).await?,
                "caddy-preview" => {
                    let _guard = state.0.operations.lock().await;
                    crate::caddy::preview(state, &args(data)?).await?
                }
                "caddy-adopt" => {
                    let _guard = state.0.operations.lock().await;
                    json!({"base":crate::caddy::adopt(state).await?})
                }
                "caddy-apply" => {
                    let _guard = state.0.operations.lock().await;
                    crate::caddy::apply(state).await?;
                    ok()
                }
                "diagnostics" => super::diagnostics(state).await?,
                "backup" => json!({"file":super::backup(state).await?}),
                "storage" => admin::storage(state, user, args(data)?).await?,
                _ => return Err(Error::bad("Unknown action.")),
            }
        }
    };
    // Nothing has changed yet when the person is first asked to confirm.
    if output.get("requires_confirmation").is_none() {
        state.audit(&user.id, &action, &id).await?;
    }
    Ok(Json(output))
}
