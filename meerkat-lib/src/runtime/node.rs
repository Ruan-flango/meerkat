//! This module implements the Node struct which owns the global
//! service type definitions, performs static validation checks,
//! and transitions to the runtime Manager

//! TODO: The intent of this module right now is to slowly migrate
//! from `Manager` to `Node` over time, as documented on GitHub under
//! Issue 106 "Meerkat Node and Program Representations"

use crate::error::{Error, Result};
use crate::runtime::ast::Stmt;
use crate::runtime::interner::Interner;
use crate::runtime::tt::types::ServiceType;
use crate::runtime::{nameres, Env, Manager};

/// Root manager for compiling and executing a Meerkat node
pub struct Node<'a> {
    pub service_classes: Env<'a, ServiceType<'a>>,
    pub interner: Interner,
}

impl<'a> Node<'a> {
    /// Create a new empty Node representing the process context
    pub fn new() -> Self {
        Node {
            service_classes: Env::new(None),
            interner: Interner::new(),
        }
    }

    /// Load parsed statements from a file path
    ///
    /// Args:
    ///     `path` (`&str`): The file path to parse
    ///
    /// Returns:
    ///     `Result<Vec<Stmt>>`: The parsed statements, or an error
    pub fn load_file(&mut self, path: &str) -> Result<Vec<Stmt>> {
        crate::runtime::parser::parse_file(path, &mut self.interner)
            .map_err(|e| Error::Message(e.to_string()))
    }

    /// Perform static analysis checks on the parsed service
    /// declarations
    ///
    /// Args:
    ///     `program` (`&[Stmt]`): The parsed program statements
    ///
    /// Returns:
    ///     `Result<()>`: Ok if checks pass, or an error
    pub fn check(&self, program: &[Stmt]) -> Result<()> {
        nameres::resolve(program).map_err(|e| match e {
            nameres::Error::UnknownIdentifier {
                name,
                expected,
                context_name,
            } => {
                let name_str = self.interner.get(name);
                let msg = match context_name {
                    Some(ctx) => {
                        let ctx_str = self.interner.get(ctx);
                        format!(
                            "Unknown identifier '{}' (expected {}) \
                             in service '{}'",
                            name_str, expected, ctx_str
                        )
                    }
                    None => {
                        format!("Unknown identifier '{}' (expected {})", name_str, expected)
                    }
                };
                Error::Message(msg)
            }
            other => Error::Message(other.to_string()),
        })
    }

    /// Start the runtime manager consuming this Node
    ///
    /// Returns:
    ///     `Manager`: The running manager instance
    pub fn start(self) -> Manager {
        Manager::new(self.interner)
    }
}

impl<'a> Default for Node<'a> {
    /// Create a new empty Node representing the process context
    fn default() -> Self {
        Self::new()
    }
}
