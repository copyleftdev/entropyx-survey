//! Library surface of the exbridge server.
//!
//! `pipeline` is public so `tests/parity.rs` can run the re-hosted scan
//! directly and diff it against the installed `entropyx` binary.
//! `fleets` groups HEAD paths into peer sets and runs WhatTheDiff over
//! each, producing the divergence layer the terrain overlays.
//! `people` resolves commit addresses to identities via kraken, and
//! reports how much of the history that enrichment actually covers.

pub mod brandes;
pub mod fleets;
pub mod people;
pub mod pipeline;
pub mod sse;
