//! Compatibility routes for clients using the original Save sync API.
use crate::{Result, State, auth, device_codes, devices, sync};
use axum::{Json, extract::State as ExtractState, http::HeaderMap};
use serde_json::{Value, json};
pub async fn pair(
    ExtractState(state): ExtractState<State>,
    axum::Extension(ip): axum::Extension<auth::ClientIp>,
    Json(input): Json<devices::DeviceInfo>,
) -> Result<Json<Value>> {
    devices::pair_for(&state, &ip, input, device_codes::SYNC).await
}
pub async fn poll(
    ExtractState(state): ExtractState<State>,
    Json(input): Json<devices::Poll>,
) -> Result<Json<Value>> {
    devices::poll_for(&state, device_codes::SYNC, &input.device_code).await
}
pub async fn login(
    ExtractState(state): ExtractState<State>,
    axum::Extension(ip): axum::Extension<auth::ClientIp>,
    Json(input): Json<devices::Login>,
) -> Result<Json<Value>> {
    devices::login_for(&state, &ip, input, true).await
}
pub async fn me(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let caller = sync::caller(&state, &headers).await?;
    let Json(mut value) = devices::me(ExtractState(state.clone()), headers).await?;
    value["quota"] = json!({"used":sync::usage(&state,&caller.user.id).await?,"limit":sync::quota(&state,&caller.user.id).await?});
    value["max_file_bytes"] = json!(sync::max_file(&state).await);
    Ok(Json(value))
}
pub use crate::devices::sign_out;
