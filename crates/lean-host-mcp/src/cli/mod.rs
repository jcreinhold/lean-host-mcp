//! CLI subcommand wiring.
//!
//! The default subcommand is `serve`, which boots the rmcp stdio
//! transport against a [`ProjectBroker`](crate::broker::ProjectBroker).
//! [`install`] makes one Lake root ready to serve: worker, writer, server
//! binary, store and local config. [`install_worker`] builds and installs a
//! per-toolchain worker binary into a worker directory
//! ([`workers_dir`](crate::toolchain::workers_dir)). [`config_init`] writes a
//! documented starter config file.

pub mod config_init;
pub mod install;
pub mod install_worker;
mod installed_shims;
pub mod processes;
