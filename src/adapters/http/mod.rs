//! HTTP adapter: OpenAPI contract resolution, request building, transport.
//!
//! # Design notes
//!
//! **Chosen:** the contract module owns OpenAPI loading and operation
//! extraction. It returns Nadir's `ResolvedContract`/`ResolvedOperation`
//! types, hiding the raw OpenAPI JSON from callers. The transport module
//! wraps `ureq` and enforces loopback-only destinations.

pub mod contract;
pub mod request;
pub mod transport;

pub(crate) mod execution;
pub(crate) mod template;
