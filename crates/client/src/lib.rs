//! IPC client library. Used by `devstack-tui` and `devstack-cli` to talk to
//! `devstack-supervisor` and `devstack-shared-supervisor` over Unix sockets.
//!
//! Wire format: length-prefixed JSON-lines envelope, see ADR-0008.
