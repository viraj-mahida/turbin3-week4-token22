//! Remittance stablecoin on Token-2022.
//!
//! * [`remit::RemitV1`]: public mint with transfer fee, metadata, default-frozen
//!   accounts and a mint close authority.
//! * [`remit::RemitV2`]: the same mint plus a permanent delegate and
//!   confidential transfers (with confidential transfer fees).
//!
//! * [`agent`]: client for the `remit-agent` program (delegated transfers via
//!   CPI, used for the CPI Guard challenge).
//!
//! The tests in `tests/` drive both mints end to end against the real
//! Token-2022 program running inside `solana-program-test`.

pub mod agent;
pub mod confidential;
pub mod env;
pub mod remit;
pub mod state;

pub use env::{Env, RemitToken};
