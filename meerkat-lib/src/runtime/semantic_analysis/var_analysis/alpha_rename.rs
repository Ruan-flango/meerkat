use std::collections::{HashMap, HashSet};

use crate::ast::{ActionStmt, Expr};
use crate::runtime::interner::Symbol;

impl Expr {
    /// Alpha renaming of expression `self`
    ///
    /// Rename variables if they are free in expression `self`
    ///
    /// Args:
    ///     `var_binded` (`&HashSet<Symbol>`): The set of bound symbols
    ///     `renames` (`&HashMap<Symbol, Symbol>`): The map of renames to apply
    pub fn alpha_rename(
        &mut self,
        var_binded: &HashSet<Symbol>,
        renames: &HashMap<Symbol, Symbol>,
    ) {
        match self {
            Expr::Literal { .. } => {}
            Expr::Variable { name } => {
                if !var_binded.contains(name) && renames.contains_key(name) {
                    *name = *renames.get(name).unwrap();
                }
            }
            Expr::KeyVal { value, .. } => {
                value.alpha_rename(var_binded, renames);
            }
            Expr::Tuple { val } => {
                for item in val {
                    item.alpha_rename(var_binded, renames);
                }
            }
            Expr::Unop { expr, .. } => {
                expr.alpha_rename(var_binded, renames);
            }
            Expr::Binop { expr1, expr2, .. } => {
                expr1.alpha_rename(var_binded, renames);
                expr2.alpha_rename(var_binded, renames);
            }
            Expr::If { cond, expr1, expr2 } => {
                cond.alpha_rename(var_binded, renames);
                expr1.alpha_rename(var_binded, renames);
                expr2.alpha_rename(var_binded, renames);
            }
            Expr::Func { params, body, .. } => {
                let mut new_binds = var_binded.clone();
                new_binds.extend(params.iter().map(|p| p.name));
                body.alpha_rename(&new_binds, renames);
            }
            Expr::Call { func, args } => {
                func.alpha_rename(var_binded, renames);
                for arg in args {
                    arg.alpha_rename(var_binded, renames);
                }
            }
            Expr::Html(template) => {
                for e in template.embedded_exprs_mut() {
                    e.alpha_rename(var_binded, renames);
                }
            }

            Expr::Action(stmts) => {
                for stmt in stmts {
                    stmt.alpha_rename(var_binded, renames);
                }
            }
            Expr::MemberAccess { .. } => {}
            Expr::Select { where_clause, .. } => {
                where_clause.alpha_rename(var_binded, renames);
            }
            Expr::Table { records, .. } => {
                for record in records {
                    record.alpha_rename(var_binded, renames);
                }
            }
            Expr::Fold {
                operation,
                identity,
                ..
            } => {
                operation.alpha_rename(var_binded, renames);
                identity.alpha_rename(var_binded, renames);
            }
        }
    }
}

impl ActionStmt {
    /// Perform alpha renaming on an `ActionStmt`
    ///
    /// Args:
    ///     `_var_binded` (`&HashSet<Symbol>`): The set of bound symbols
    ///     `_renames` (`&HashMap<Symbol, Symbol>`): The map of renames to apply
    pub fn alpha_rename(
        &mut self,
        _var_binded: &HashSet<Symbol>,
        _renames: &HashMap<Symbol, Symbol>,
    ) {
        // TODO: Implement alpha renaming for `ActionStmt` based on its structure
        // This is a placeholder; adjust based on `ActionStmt`'s actual fields
        panic!("alpha_rename for ActionStmt is not implemented yet");
    }
}
