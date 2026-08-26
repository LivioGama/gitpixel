//! gitpixel-serve — transport-agnostic service (`api`) and the Unix-socket
//! NDJSON daemon with fs watching (`daemon`).

pub mod api;
pub mod daemon;

pub use api::{Request, Response, ServeError, Service};
pub use daemon::{pid_path, run, socket_path};
