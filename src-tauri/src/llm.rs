//! The provider calls, shared with the gateway.
//!
//! The code lives in the `vd-llm` crate so the server can send the same
//! request the desktop does; this module keeps the old path (`crate::llm::…`)
//! pointing at it.

pub use vd_llm::*;
