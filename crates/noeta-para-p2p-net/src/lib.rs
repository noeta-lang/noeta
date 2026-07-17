//! The real p2panda transport for `para.p2p`/`para.synced` (para-namespace follow-on F2b).
//!
//! A [`P2pNode`] is a live p2panda-net node — gossip + log-sync over iroh/QUIC — that implements the
//! [`noeta_ext_abi::host::P2p`] capability with genuine peer delivery, the non-loopback backing the
//! deterministic in-process broker stands in for. It owns its **own** multi-thread tokio runtime (its
//! gossip/sync background tasks must outlive the individual synchronous dispatches that drive it), so
//! it is entirely self-contained: construct it, use it, drop it (which tears the node and its runtime
//! down). [`p2p_crypto`] holds the p2panda-spaces group-encryption assembly the node drives for an
//! end-to-end-encrypted `synced_signal`.
//!
//! This crate depends only on `noeta-ext-abi` and the p2panda/tokio tree — never on `noeta-host-real`,
//! `noeta-stdlib`, or the extension crate — so the `para.p2p` extension can own the node without any
//! dependency cycle, and `noeta-host-real` sheds the whole iroh/QUIC tree.

pub mod p2p_crypto;
pub mod p2p_node;

pub use p2p_node::{P2pConfig, P2pNode};

/// An IO-kind [`StdError`](noeta_ext_abi::StdError) with `message` — the transport's single error
/// constructor (the node's operations are host effects; every failure surfaces as an `Io` error).
pub(crate) fn io_error(message: String) -> noeta_ext_abi::StdError {
    noeta_ext_abi::StdError {
        kind: noeta_ext_abi::ErrorKind::Io,
        message,
    }
}
