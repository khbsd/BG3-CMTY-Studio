//! Platform connectivity infrastructure (Nexus Mods, mod.io).
//!
//! Provides shared utilities for credential management, HTTP requests,
//! rate limiting, progress reporting, input validation, and ZIP packaging.
//!
//! The `nexus` and `modio` submodules are feature-gated behind
//! `nexus-integration` and `modio-integration` respectively.

pub mod credentials;
pub mod deps_suggest;
pub mod errors;
pub mod http;
pub mod packaging;
pub mod progress;
pub mod rate_limiter;
pub mod validation;

// Feature-gated platform implementations
#[cfg(feature = "modio-integration")]
pub mod modio;
#[cfg(feature = "nexus-integration")]
pub mod nexus;
