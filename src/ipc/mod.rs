//! Local IPC between the daemon and its clients (Navin Desktop, CLI).
//!
//! Transport: Unix domain socket (line-delimited JSON). Windows named-pipe
//! support plugs in behind the same protocol when the engine ships there.

pub mod events;
pub mod protocol;
pub mod server;
