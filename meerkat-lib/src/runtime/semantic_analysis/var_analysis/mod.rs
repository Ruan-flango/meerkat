//! dependency analysis for var/def node in meerkat
//!

use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
};

use crate::ast;
use crate::runtime::interner::Symbol;

pub mod alpha_rename;
pub mod dep_analysis;
pub mod read_write;

pub struct DependAnalysis {
    pub vars: HashSet<Symbol>,
    pub defs: HashSet<Symbol>,
    pub tables: HashSet<Symbol>,
    pub dep_graph: HashMap<Symbol, HashSet<Symbol>>,
    /// Topological order of variables and definitions represented by `Symbol`
    pub topo_order: Vec<Symbol>,
    /// Transitively dependent variables and definitions of a name
    pub dep_transitive: HashMap<Symbol, HashSet<Symbol>>,
    /// Transitively dependent variables of a name
    ///
    /// The `dep_vars` map values are a subset of `dep_transitive`
    pub dep_vars: HashMap<Symbol, HashSet<Symbol>>,
}

/// Implement the `Display` trait for the `DependAnalysis` struct
///
/// This formats and displays the dependency graph, transitive
/// dependencies, and topological order
impl Display for DependAnalysis {
    /// Format the dependency analysis result for display
    ///
    /// Args:
    ///     `f` (`&mut std::fmt::Formatter<'_>`): The formatter target
    ///
    /// Returns:
    ///     `std::fmt::Result`: The result of the formatting operation
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Dependency Graph ")?;
        for (name, deps) in self.dep_graph.iter() {
            write!(f, "{} -> ", name)?;
            for dep in deps.iter() {
                write!(f, "{},", dep)?;
            }
            writeln!(f)?;
        }
        writeln!(f, "Transitive Dependency (Var only) ")?;
        for (name, deps) in self.dep_vars.iter() {
            write!(f, "{} -> ", name)?;
            for dep in deps.iter() {
                write!(f, "{},", dep)?;
            }
            writeln!(f)?;
        }

        writeln!(f, "Topological Order ")?;
        for name in self.topo_order.iter() {
            write!(f, "{} ", name)?;
        }
        writeln!(f)?;
        Ok(())
    }
}

/// Calculate dependencies for a service from its declarations
///
/// Args:
///     `decls` (`&[ast::Decl]`): The service declarations
///
/// Returns:
///     `DependAnalysis`: The calculated dependency analysis state
pub fn calc_dep_srv(decls: &[ast::Decl]) -> DependAnalysis {
    let mut da = DependAnalysis::new(decls);
    da.calc_dep_vars();
    //println!("{}", da);

    da
}
