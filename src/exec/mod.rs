//! Chain execution: walking a [`Decision`](crate::route::Decision)'s legs and
//! keeping the per-backend health that decides which legs are worth trying.
//!
//! Nothing here knows how the legs were chosen, and nothing in `route/` knows how
//! they run. That boundary is the point of the split.

pub mod chain;
pub mod gate;
