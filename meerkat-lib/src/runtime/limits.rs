//! System limits for Meerkat

/// Maximum allowed length of an identifier in characters
pub const MAX_IDENTIFIER_LENGTH: usize = 64;

/// Maximum allowed length of a string literal in characters
pub const MAX_STRING_LITERAL_LENGTH: usize = 8192;

/// Maximum allowed nesting depth of a type structure during deserialization
pub const MAX_TYPE_DEPTH: usize = 16;

/// #39: max length for network-request path/address strings (file paths and
/// reply addresses). Longer than an identifier but bounded, so a client cannot
/// send an unbounded path or reply_to. Source code in responses is unbounded.
pub const MAX_NET_REQUEST_STRING_LENGTH: usize = 4096;

/// Maximum nesting depth of scope blocks to prevent stack overflows
pub const MAX_SCOPE_DEPTH: usize = 128;
