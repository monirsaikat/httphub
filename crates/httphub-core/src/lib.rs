//! httphub-core: the engine every face of httphub is built on.
//!
//! Hard rules for this crate:
//!   * No terminal output, no Tauri, no UI assumptions. Structured types only.
//!   * Every long-running operation reports progress as events, never as a
//!     single return value, so a CLI waterfall and a GUI waterfall can be
//!     rendered from the same stream.
//!   * Response bodies stream to disk and are handed back as a path, never
//!     buffered into memory for the caller.

pub mod engine;
pub mod error;
pub mod event;
pub mod expect;
pub mod format;
pub mod model;
pub mod redirect;
pub mod run;
pub mod tls;
pub mod transport;
pub mod vars;

pub use error::{Error, Result};
pub use event::{EventSink, RequestEvent, Timings};
pub use model::{ByteCounts, RequestSpec, ResponseHandle};
