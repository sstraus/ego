//! Crate-private skill admission contract.
//!
//! Production parsing, discovery, and admission types will be owned by this
//! module in the final implementation. The RED contract is kept in a unit
//! test child so it cannot become a public service surface.

#[cfg(test)]
mod tests;
