//! Typed Rust services for portable LOKI WASI plugins.
//!
//! ```ignore
//! use loki_sdk::{Request, Response, Result};
//! fn handle(request: Request) -> Result<Response> {
//!     Response::json(&request.context().user)
//! }
//! loki_sdk::export!(handle);
//! ```
mod error;
pub use error::{Error, ErrorKind, Result};
#[cfg(feature = "guest")]
mod storage;
#[cfg(feature = "guest")]
pub use storage::*;
#[cfg(feature = "guest")]
pub mod fetch;
#[cfg(feature = "guest")]
mod guest;
#[cfg(feature = "manifest")]
pub mod manifest;
pub mod panel;
#[cfg(feature = "realtime")]
pub mod realtime;
#[cfg(feature = "manifest")]
pub mod registry;
#[cfg(feature = "guest")]
pub use guest::*;

/// Parameter lists remain separate from SQL; values are never interpolated.
#[macro_export]
macro_rules! params {
    ($($value:expr),* $(,)?) => {$crate::__private::serde_json::json!([$($value),*])};
}

#[doc(hidden)]
pub mod __private {
    #[cfg(feature = "guest")]
    pub use crate::guest::serve;
    pub use serde_json;
    #[cfg(feature = "guest")]
    pub use wasi;
}

/// Export a plain Rust request handler as a standard WASI HTTP component.
#[cfg(feature = "guest")]
#[macro_export]
macro_rules! export {
    ($handler:path) => {
        struct __LokiComponent;
        impl $crate::__private::wasi::exports::http::incoming_handler::Guest for __LokiComponent {
            fn handle(
                request: $crate::__private::wasi::http::types::IncomingRequest,
                response: $crate::__private::wasi::http::types::ResponseOutparam,
            ) {
                $crate::__private::serve(request, response, $handler);
            }
        }
        $crate::__private::wasi::http::proxy::export!(__LokiComponent);
    };
}
