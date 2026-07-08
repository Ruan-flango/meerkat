use crate::ast::{ActionStmt, Expr};
use crate::runtime::interner::Symbol;
use std::collections::HashSet;

impl Expr {
    /// Collect every cross-service reference `(service, member)` appearing
    /// anywhere in this expression. Counterpart to `free_var`, which drops
    /// `MemberAccess`; issue #24 needs these to know which remote members a
    /// def must subscribe to. Symbols are interner-local, resolved to strings
    /// only at the network boundary.
    pub fn cross_service_deps(&self) -> HashSet<(Symbol, Symbol)> {
        match self {
            Expr::Literal { .. } | Expr::Variable { .. } => HashSet::new(),
            // Tables are not yet supported in cross-service dependency analysis;
            // they carry no MemberAccess today, so an empty set is correct for now.
            Expr::Table { .. } => HashSet::new(),
            Expr::MemberAccess {
                service_name,
                member_name,
            } => HashSet::from([(*service_name, *member_name)]),
            Expr::KeyVal { value, .. } => value.cross_service_deps(),
            Expr::Tuple { val } => {
                let mut deps = HashSet::new();
                for item in val {
                    deps.extend(item.cross_service_deps());
                }
                deps
            }
            Expr::Unop { expr, .. } => expr.cross_service_deps(),
            Expr::Binop { expr1, expr2, .. } => {
                let mut deps = expr1.cross_service_deps();
                deps.extend(expr2.cross_service_deps());
                deps
            }
            Expr::If { cond, expr1, expr2 } => {
                let mut deps = cond.cross_service_deps();
                deps.extend(expr1.cross_service_deps());
                deps.extend(expr2.cross_service_deps());
                deps
            }
            Expr::Func { body, .. } => body.cross_service_deps(),
            Expr::Html(template) => {
                let mut deps = HashSet::new();
                for e in template.embedded_exprs() {
                    deps.extend(e.cross_service_deps());
                }
                deps
            }
            Expr::Call { func, args } => {
                let mut deps = func.cross_service_deps();
                for arg in args {
                    deps.extend(arg.cross_service_deps());
                }
                deps
            }
            Expr::Action(stmts) => {
                let mut deps = HashSet::new();
                for stmt in stmts {
                    deps.extend(cross_service_deps_in_action_stmt(stmt));
                }
                deps
            }
            Expr::Select { where_clause, .. } => where_clause.cross_service_deps(),
            Expr::Fold {
                operation,
                identity,
                ..
            } => {
                let mut deps = operation.cross_service_deps();
                deps.extend(identity.cross_service_deps());
                deps
            }
            Expr::List(exprs) => {
                let mut deps = HashSet::new();
                for expr in exprs {
                    deps.extend(expr.cross_service_deps());
                }
                deps
            }
            Expr::Range { start, end } => {
                let mut deps = start.cross_service_deps();
                deps.extend(end.cross_service_deps());
                deps
            }
        }
    }

    /// Returns the free variables in `self` with respect to `var_binded`
    ///
    /// This is used for:
    /// - Extracting dependencies of each `def` declaration
    /// - Evaluating an expression (substitution based evaluation)
    ///
    /// Args:
    ///     `reactive_names` (`&HashSet<Symbol>`): The set of reactive names
    ///     `var_binded` (`&HashSet<Symbol>`): The set of bound symbols
    ///
    /// Returns:
    ///     `HashSet<Symbol>`: The set of free variables
    pub fn free_var(
        &self,
        reactive_names: &HashSet<Symbol>,
        var_binded: &HashSet<Symbol>,
    ) -> HashSet<Symbol> {
        match self {
            Expr::Literal { .. } | Expr::Table { .. } => HashSet::new(),
            Expr::Variable { name } => {
                if var_binded.contains(name) {
                    HashSet::new()
                } else {
                    HashSet::from([*name])
                }
            }
            Expr::KeyVal { value, .. } => value.free_var(reactive_names, var_binded),
            Expr::Tuple { val } => {
                let mut free_vars = HashSet::new();
                for item in val {
                    free_vars.extend(item.free_var(reactive_names, var_binded));
                }
                free_vars
            }
            Expr::Unop { op: _, expr } => expr.free_var(reactive_names, var_binded),
            Expr::Binop {
                op: _,
                expr1,
                expr2,
            } => {
                let mut free_vars = expr1.free_var(reactive_names, var_binded);
                free_vars.extend(expr2.free_var(reactive_names, var_binded));
                free_vars
            }
            Expr::If { cond, expr1, expr2 } => {
                let mut free_vars = cond.free_var(reactive_names, var_binded);
                free_vars.extend(expr1.free_var(reactive_names, var_binded));
                free_vars.extend(expr2.free_var(reactive_names, var_binded));
                free_vars
            }
            Expr::Func { params, body, .. } => {
                let mut new_binds = var_binded.clone();
                new_binds.extend(params.iter().map(|p| p.name));
                body.free_var(reactive_names, &new_binds)
            }
            Expr::Html(template) => {
                let mut free_vars = HashSet::new();
                for e in template.embedded_exprs() {
                    free_vars.extend(e.free_var(reactive_names, var_binded));
                }
                free_vars
            }
            Expr::Call { func, args } => {
                let mut free_vars = func.free_var(reactive_names, var_binded);
                for arg in args {
                    free_vars.extend(arg.free_var(reactive_names, var_binded));
                }
                free_vars
            }
            Expr::Action(stmts) => {
                let mut free_vars = HashSet::new();
                for stmt in stmts {
                    free_vars.extend(free_vars_in_action_stmt(stmt, reactive_names, var_binded));
                }
                free_vars.difference(reactive_names).cloned().collect()
            }
            Expr::MemberAccess { .. } => {
                // member access on another service - no local free vars
                HashSet::new()
            }
            Expr::Select {
                table_name,
                where_clause,
                ..
            } => {
                let mut free_vars = where_clause.free_var(reactive_names, var_binded);
                free_vars.insert(*table_name);
                free_vars
            }
            Expr::Fold {
                operation,
                identity,
                ..
            } => {
                let mut free_vars = HashSet::new();
                free_vars.extend(operation.free_var(reactive_names, var_binded));
                free_vars.extend(identity.free_var(reactive_names, var_binded));
                free_vars
            }
            Expr::List(val) => {
                let mut free_vars = HashSet::new();
                for item in val {
                    free_vars.extend(item.free_var(reactive_names, var_binded));
                }
                free_vars
            }
            Expr::Range { start, end } => {
                let mut free_vars = start.free_var(reactive_names, var_binded);
                free_vars.extend(end.free_var(reactive_names, var_binded));
                free_vars
            }
        }
    }
}

fn free_vars_in_action_stmt(
    stmt: &ActionStmt,
    reactive_names: &HashSet<Symbol>,
    var_binded: &HashSet<Symbol>,
) -> HashSet<Symbol> {
    match stmt {
        ActionStmt::Assign { expr, .. } => expr.free_var(reactive_names, var_binded),
        ActionStmt::Do(expr) => expr.free_var(reactive_names, var_binded),
        ActionStmt::Assert(expr, _) => expr.free_var(reactive_names, var_binded),
        ActionStmt::Let { expr, .. } => expr.free_var(reactive_names, var_binded),
        ActionStmt::Expr(expr) => expr.free_var(reactive_names, var_binded),
        ActionStmt::Insert { row, .. } => row.free_var(reactive_names, var_binded),
        ActionStmt::For { var, iterable, body } => {
            let mut free_vars = iterable.free_var(reactive_names, var_binded);
            let mut body_binds = var_binded.clone();
            body_binds.insert(*var);
            for s in body {
                free_vars.extend(free_vars_in_action_stmt(s, reactive_names, &body_binds));
            }
            free_vars
        }
    }
}

fn cross_service_deps_in_action_stmt(stmt: &ActionStmt) -> HashSet<(Symbol, Symbol)> {
    match stmt {
        ActionStmt::Let { expr, .. } => expr.cross_service_deps(),
        ActionStmt::Expr(expr) => expr.cross_service_deps(),
        ActionStmt::Do(expr) => expr.cross_service_deps(),
        ActionStmt::Assert(expr, _) => expr.cross_service_deps(),
        ActionStmt::Assign { expr, .. } => expr.cross_service_deps(),
        ActionStmt::Insert { row, .. } => row.cross_service_deps(),
        ActionStmt::For { iterable, body, .. } => {
            let mut deps = iterable.cross_service_deps();
            for s in body {
                deps.extend(cross_service_deps_in_action_stmt(s));
            }
            deps
        }
    }
}

#[cfg(test)]
mod html_dep_tests {
    use crate::ast::Expr;
    use crate::runtime::html::HtmlTemplateBuilder;
    use crate::runtime::interner::Interner;
    use std::collections::HashSet;

    /// #39: the interpolated expression in an html template must surface as a
    /// free variable, so the html def registers a dependency and re-renders
    /// when that dependency changes (issue #24 propagation).
    #[test]
    fn test_html_free_var_tracks_interpolation() {
        let mut interner = Interner::new();
        let count = interner.insert("count");
        let mut builder = HtmlTemplateBuilder::new();
        builder.push_text("<p>");
        builder.push_expr(Expr::Variable { name: count });
        builder.push_text("</p>");
        let expr = Expr::Html(builder.build());
        let free = expr.free_var(&HashSet::new(), &HashSet::new());
        assert!(
            free.contains(&count),
            "html interpolation must be a free var: {:?}",
            free
        );
    }

    /// #39: interpolations referencing another service surface as cross-service
    /// dependencies too.
    #[test]
    fn test_html_cross_service_deps_tracks_interpolation() {
        let mut interner = Interner::new();
        let svc = interner.insert("counter");
        let member = interner.insert("count");
        let mut builder = HtmlTemplateBuilder::new();
        builder.push_expr(Expr::MemberAccess {
            service_name: svc,
            member_name: member,
        });
        let expr = Expr::Html(builder.build());
        let deps = expr.cross_service_deps();
        assert!(
            deps.contains(&(svc, member)),
            "html interpolation must surface cross-service dep: {:?}",
            deps
        );
    }
}
