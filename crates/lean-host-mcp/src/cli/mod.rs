//! CLI subcommand wiring.
//!
//! The default subcommand is `serve`, which boots the rmcp stdio
//! transport against a [`ProjectBroker`](crate::broker::ProjectBroker).
//! [`install_worker`] (sub)builds and installs a per-toolchain worker
//! binary into [`WorkerBinary::install_root`](crate::toolchain::WorkerBinary::install_root).
//! [`config_init`] writes a documented starter config file.

pub mod config_init;
pub mod install_worker;
