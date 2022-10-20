pub use wit_bindgen_host_wasmtime_rust_macro::*;

#[cfg(feature = "tracing-lib")]
pub use tracing_lib as tracing;
#[doc(hidden)]
pub use {anyhow, async_trait::async_trait, wasmtime};

use std::fmt::{self, Debug, Display};

pub struct ApiError<T>(T);
impl<T> ApiError<T> {
    pub fn new(t: T) -> Self {
        Self(t)
    }
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T: Debug> Debug for ApiError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl<T: Debug> Display for ApiError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ApiError({:?})", self.0)
    }
}
impl<T: Debug> std::error::Error for ApiError<T> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}
