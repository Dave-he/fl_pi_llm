//! fl_pi_llm: pi-agent-core and pi-ai on QuickJS, bridged to Dart ([`api`])
//! or hosted by a Rust program ([`host`]).

#[cfg(feature = "frb")]
pub mod api;
pub mod engine;
pub mod memory;
#[cfg(feature = "frb")]
mod frb_generated;
#[cfg(feature = "host")]
pub mod host;
