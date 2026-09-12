//! The backend-neutral half of Dekopon's telemetry-query providers.
//!
//! #250 decided one component per backend over one core crate, and this is that crate: the command
//! word grammar (`openobserve`/`quickwit`/… plus the neutral `agent` and `broker`), the 30-day
//! window cap, the dekopon column projection and its exclusion list, the fitting that turns an
//! oversize result into `truncated` with an exact `omittedRows`, and the output shapes every
//! backend must produce byte-for-byte in schema. What it does not contain is a wire protocol: a
//! backend implements [`backend::Backend`]'s `plan` and `fold` and owns its own SQL, query strings,
//! or data frames.
//!
//! Nothing here is a security bound. The broker owns authorization, egress, and credential
//! injection; every bound in this crate is a courtesy to the caller and a bound on what this guest
//! will *ask for*. The host's refusals stay the backstop, and the tests say which is which.
//!
//! Unlike dekopon's own crates a guest cannot `#![forbid(unsafe_code)]` end to end — the generated
//! component bindings contain `unsafe` by construction — but this crate generates none, so it can.

#![forbid(unsafe_code)]

pub mod backend;
pub mod fit;
pub mod grammar;
pub mod output;
pub mod projection;
pub mod query;
pub mod table;
pub mod window;

pub use backend::{Backend, run};
pub use grammar::Capabilities;
pub use query::{Query, QueryError};
