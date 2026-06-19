use crate::net::ast::{NetActionStmt, NetValue};
use crate::runtime::txn::TxnId;
use serde::{Deserialize, Serialize};

/// Unique identifier for sent messages (for error tracking)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MessageId(pub u64);

/// Canonical internet-routable address represented by `Address`
///
/// Examples:
/// - Server: `/ip4/203.0.113.10/tcp/9000/p2p/12D3...`
/// - Client: `/ip4/203.0.113.10/tcp/9000/p2p/server-id/ws/p2p/client-id`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct Address(pub String);

impl Address {
    /// Creates a new `Address` from the given string
    pub fn new(addr: impl Into<String>) -> Self {
        Address(addr.into())
    }
}

/// Globally unique identity of a service (as opposed to a node)
///
/// Per the `Historiographer` design, a service identity is its global,
/// internet-routable network address including the service slug, e.g.,
/// `/ip4/203.0.113.10/tcp/9000/p2p/12D3.../my_service`
/// This is used as part of the key for all transaction state that
/// mentions a variable, so that the same variable name in two
/// different services (possibly on different nodes) never collides
///
/// For local-only execution where the node has no network address yet,
/// the identity falls back to the service name. On a single node
/// names are unambiguous; the address-based identity takes over once
/// networking is available (and is required for cross-node
/// transactions)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ServiceNetId(pub String);

impl ServiceNetId {
    /// Creates a new `ServiceNetId` from the given string
    pub fn new(id: impl Into<String>) -> Self {
        ServiceNetId(id.into())
    }
}

/// Message types in the `Meerkat` protocol
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MeerkatMessage {
    /// Ping for testing
    Ping { content: String },

    /// Pong response
    Pong { content: String },

    /// Peer announcement with their canonical `Address`
    Announce { peer_addr: Address },

    /// Transaction message (for future use)
    Transaction { tx_id: u64, payload: Vec<u8> },

    /// Propagation message (for future use)
    Propagation { var_id: u64, new_value: Vec<u8> },

    /// Request to look up a member of a service on a remote node
    LookupRequest {
        request_id: u64,
        service: String,
        member: String,
        reply_to: String,
        /// When `Some`, the read joins this transaction: the owning node
        /// acquires and holds a read lock under the shared `txn_id`
        /// until `commit`/`abort`
        /// When `None`, it is a plain unlocked read
        txn_id: Option<TxnId>,
    },

    /// Response to a `LookupRequest` with the serialized value
    LookupResponse { request_id: u64, value: NetValue },

    /// Response indicating lookup failed
    LookupError { request_id: u64, error: String },

    /// Execute an action on a remote service
    ActionRequest {
        request_id: u64,
        service: String,
        stmts: Vec<NetActionStmt>,
        env: Vec<(String, NetValue)>,
        reply_to: String,
        /// When `Some`, the action joins the originator's distributed
        /// transaction: execute under this shared `txn_id` and hold
        /// (do not commit) until a later `Commit`/`Abort`
        /// When `None`, execute standalone and commit immediately
        txn_id: Option<TxnId>,
    },

    /// Response to `ActionRequest`
    ActionResponse {
        request_id: u64,
        success: bool,
        error: Option<String>,
    },

    /// Tell a participant node to commit a distributed transaction:
    /// apply the writes it buffered for `txn_id` and release the
    /// locks it holds
    Commit {
        request_id: u64,
        txn_id: TxnId,
        reply_to: String,
    },

    /// Acknowledgement that a `Commit` was applied (or failed) on a
    /// participant
    CommitResponse {
        request_id: u64,
        success: bool,
        error: Option<String>,
    },

    /// Tell a participant node to abort a distributed transaction:
    /// discard the writes it buffered for `txn_id` and release the
    /// locks it holds
    /// Acknowledged so the originator knows the participant's locks
    /// are freed before it returns (and exits)
    Abort {
        request_id: u64,
        txn_id: TxnId,
        reply_to: String,
    },

    /// Acknowledgement that an `Abort` was processed on a participant
    AbortResponse { request_id: u64 },

    /// Sent by an owner when it parks a transactional request on a
    /// wait queue (wait-die wait): tells the waiting originator the
    /// request is alive and still queued, so it keeps waiting
    /// instead of timing out
    WaitParked { request_id: u64 },
}

/// Errors that can occur when sending messages
#[derive(Debug, Clone)]
pub enum SendError {
    /// Could not resolve/reach the `Address`
    UnreachableAddress(Address),

    /// Connection dropped before send completed
    ConnectionLost(String),

    /// Message too large or other protocol error
    ProtocolError(String),
}

/// Describes what kind of node we are
///
/// Determines how `translate_address` behaves
pub enum NodeType {
    /// Server node - can dial IP directly, no translation needed
    Server,
    /// Browser client - can only reach the network via WebSocket
    /// to relay server
    BrowserClient {
        /// WebSocket address of our relay server, e.g.,
        /// `/ip4/server1-ip/tcp/9001/ws/p2p/server1-id`
        relay_server: Address,
    },
}
