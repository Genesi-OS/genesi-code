//! Re-export of the preview compiler, which lives in its own crate.
//!
//! The compiler is UI-free and is kept out of this crate so its tests link a
//! small binary; this shim keeps the `super::live_preview::…` paths the panel
//! and the renderer already use.

pub use live_preview::compiler::*;
