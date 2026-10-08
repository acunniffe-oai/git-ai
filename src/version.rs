//! Version identity shared by diagnostics and OTEL delivery.
#[cfg(all(debug_assertions, test))]
pub const GIT_AI_VERSION: &str = "development";

#[cfg(all(debug_assertions, not(test)))]
pub const GIT_AI_VERSION: &str = concat!("development:", env!("CARGO_PKG_VERSION"));

#[cfg(not(debug_assertions))]
pub const GIT_AI_VERSION: &str = env!("CARGO_PKG_VERSION");
