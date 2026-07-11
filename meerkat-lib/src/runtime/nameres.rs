//! Static name resolution analysis for the Meerkat language compiler
//!
//! This module resolves and validates all variable usages in abstract
//! syntax trees, failing defensively if an unbound symbol is accessed

use crate::runtime::ast::{ActionStmt, Decl, Expr, Stmt, Value};
use crate::runtime::interner::Symbol;
use crate::runtime::limits::MAX_SCOPE_DEPTH;
use crate::runtime::tt::Param;
use crate::runtime::Env;
use std::collections::HashMap;
use std::fmt;

/// Errors that can occur during name resolution
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// A variable was referenced but not declared in scope
    UnboundVariable {
        /// The unbound symbol
        name: Symbol,
    },
    /// The AST nesting depth exceeded the limit
    DepthLimit,
    /// Testing an imported service is not yet supported
    ImportResolutionUnimplemented,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::UnboundVariable { name } => {
                write!(f, "Unbound variable: {}", name)
            }
            Error::DepthLimit => {
                write!(f, "Depth limit exceeded")
            }
            Error::ImportResolutionUnimplemented => {
                write!(
                    f,
                    "Name resolution for imported services \
                     is not yet implemented"
                )
            }
        }
    }
}

impl std::error::Error for Error {}

/// The stateful struct that drives static name resolution traversal
#[derive(Default)]
pub struct Resolver {
    depth: usize,
    service_members: HashMap<Symbol, Vec<Symbol>>,
}

impl Resolver {
    /// Creates a new resolver instance
    ///
    /// Returns:
    ///     `Self`: The new `Resolver` instance
    pub fn new() -> Self {
        Self {
            depth: 0,
            service_members: HashMap::new(),
        }
    }

    /// Resolves name bindings for a program represented as a slice
    /// of `Stmt`s
    ///
    /// Args:
    ///     `stmts` (`&[Stmt]`): The statements of the program
    ///     `env` (`&mut Env<'_, ()>`): The current scope environment
    ///
    /// Returns:
    ///     `Result<(), Error>`: Ok if resolution succeeds, or `Error`
    pub fn resolve_program(&mut self, stmts: &[Stmt], env: &mut Env<'_, ()>) -> Result<(), Error> {
        // Pass 1: Hoist top-level services and imports, and extract
        // service members. This pre-pass is required to support
        // mutually-referential services defined in arbitrary order.
        // By registering all service names and extracting their
        // fields into `service_members` before resolving
        // expressions in the second pass, services can refer to
        // each other out-of-order, and `@test` blocks can resolve
        // variables of their target services even if the test is
        // defined before the service
        for stmt in stmts {
            match stmt {
                Stmt::Service { name, decls } => {
                    env.bind(*name, ());
                    let mut members = Vec::new();
                    for decl in decls {
                        match decl {
                            Decl::VarDecl { name: mem, .. }
                            | Decl::DefDecl { name: mem, .. }
                            | Decl::TableDecl { name: mem, .. } => {
                                members.push(*mem);
                            }
                        }
                    }
                    self.service_members.insert(*name, members);
                }
                Stmt::Import { service_name, .. } => {
                    env.bind(*service_name, ());
                }
                Stmt::ActionStmt(_)
                | Stmt::Update { .. }
                | Stmt::Connect { .. }
                | Stmt::Test { .. }
                | Stmt::Watch { .. } => {}
            }
        }

        // Pass 2: Resolve all statements
        for stmt in stmts {
            self.resolve_stmt(stmt, env)?;
        }
        Ok(())
    }

    /// Resolves name bindings in a single statement
    ///
    /// Args:
    ///     `stmt` (`&Stmt`): The statement to resolve
    ///     `env` (`&mut Env<'_, ()>`): The current scope environment
    ///
    /// Returns:
    ///     `Result<(), Error>`: Ok if resolution succeeds, or `Error`
    fn resolve_stmt(&mut self, stmt: &Stmt, env: &mut Env<'_, ()>) -> Result<(), Error> {
        match stmt {
            Stmt::ActionStmt(action) => self.resolve_action_stmt(action, env),
            Stmt::Update {
                service_name,
                decls,
            } => {
                if env.find(*service_name).is_none() {
                    return Err(Error::UnboundVariable {
                        name: *service_name,
                    });
                }
                // A child environment is required by the contract of
                // `resolve_service` for proper hoisting and binding
                // semantics without leaking names to the outer scope
                let mut update_env = Env::new(Some(env));
                self.resolve_service(decls, &mut update_env)
            }
            Stmt::Connect { path: _, addr: _ } => Ok(()),
            Stmt::Import {
                path: _,
                service_name,
            } => {
                env.bind(*service_name, ());
                Ok(())
            }
            Stmt::Service { name, decls } => {
                env.bind(*name, ());
                let mut service_env = Env::new(Some(env));
                self.resolve_service(decls, &mut service_env)
            }
            Stmt::Test {
                service_name,
                stmts,
            } => {
                if env.find(*service_name).is_none() {
                    return Err(Error::UnboundVariable {
                        name: *service_name,
                    });
                }
                let mut test_env = Env::new(Some(env));
                match self.service_members.get(service_name) {
                    Some(members) => {
                        for member in members {
                            test_env.bind(*member, ());
                        }
                    }
                    None => {
                        return Err(Error::ImportResolutionUnimplemented);
                    }
                }
                self.resolve_action_stmts(stmts, &mut test_env)
            }
            Stmt::Watch { expr } => self.resolve_expr(expr, env),
        }
    }

    /// Resolves service-level declarations using a two-pass approach
    ///
    /// WARNING: This method hoists all declarations into the
    /// provided `env`. Callers must ensure they pass a scoped child
    /// environment if they intend to encapsulate these declarations
    /// and avoid leaking them to the outer scope
    ///
    /// Args:
    ///     `decls` (`&[Decl]`): The declarations in the service
    ///     `env` (`&mut Env<'_, ()>`): The service-level environment
    ///
    /// Returns:
    ///     `Result<(), Error>`: Ok if resolution succeeds, or `Error`
    fn resolve_service(&mut self, decls: &[Decl], env: &mut Env<'_, ()>) -> Result<(), Error> {
        for decl in decls {
            match decl {
                Decl::VarDecl {
                    name,
                    ty: _,
                    val: _,
                } => {
                    env.bind(*name, ());
                }
                Decl::DefDecl {
                    name,
                    ty: _,
                    val: _,
                    is_pub: _,
                } => {
                    env.bind(*name, ());
                }
                Decl::TableDecl { name, fields: _ } => {
                    env.bind(*name, ());
                }
            }
        }

        for decl in decls {
            match decl {
                Decl::VarDecl {
                    name: _,
                    ty: _,
                    val,
                } => {
                    self.resolve_expr(val, env)?;
                }
                Decl::DefDecl {
                    name: _,
                    ty: _,
                    val,
                    is_pub: _,
                } => {
                    self.resolve_expr(val, env)?;
                }
                Decl::TableDecl { name: _, fields: _ } => {}
            }
        }
        Ok(())
    }

    /// Resolves a list of action statements sequentially
    ///
    /// Args:
    ///     `stmts` (`&[ActionStmt]`): The action statements to resolve
    ///     `env` (`&mut Env<'_, ()>`): The current environment
    ///
    /// Returns:
    ///     `Result<(), Error>`: Ok if resolution succeeds, or `Error`
    fn resolve_action_stmts(
        &mut self,
        stmts: &[ActionStmt],
        env: &mut Env<'_, ()>,
    ) -> Result<(), Error> {
        for stmt in stmts {
            self.resolve_action_stmt(stmt, env)?;
        }
        Ok(())
    }

    /// Resolves a single action statement
    ///
    /// Args:
    ///     `stmt` (`&ActionStmt`): The action statement to resolve
    ///     `env` (`&mut Env<'_, ()>`): The current environment
    ///
    /// Returns:
    ///     `Result<(), Error>`: Ok if resolution succeeds, or `Error`
    fn resolve_action_stmt(
        &mut self,
        stmt: &ActionStmt,
        env: &mut Env<'_, ()>,
    ) -> Result<(), Error> {
        match stmt {
            ActionStmt::Let { name, ty: _, expr } => {
                self.resolve_expr(expr, env)?;
                env.bind(*name, ());
                Ok(())
            }
            ActionStmt::Expr(expr) => self.resolve_expr(expr, env),
            ActionStmt::Do(expr) => self.resolve_expr(expr, env),
            ActionStmt::Assert(expr, _text) => self.resolve_expr(expr, env),
            ActionStmt::Assign { name, expr } => {
                if env.find(*name).is_none() {
                    return Err(Error::UnboundVariable { name: *name });
                }
                self.resolve_expr(expr, env)
            }
            ActionStmt::Insert { row, table_name } => {
                if env.find(*table_name).is_none() {
                    return Err(Error::UnboundVariable { name: *table_name });
                }
                self.resolve_expr(row, env)
            }
            ActionStmt::For {
                var,
                iterable,
                body,
            } => {
                self.resolve_expr(iterable, env)?;
                let mut loop_env = Env::new(Some(env));
                loop_env.bind(*var, ());
                self.resolve_action_stmts(body, &mut loop_env)
            }
        }
    }

    /// Resolves variable names within an expression
    ///
    /// Args:
    ///     `expr` (`&Expr`): The expression to resolve
    ///     `env` (`&Env<'_, ()>`): The current environment
    ///
    /// Returns:
    ///     `Result<(), Error>`: Ok if resolution succeeds, or `Error`
    fn resolve_expr(&mut self, expr: &Expr, env: &Env<'_, ()>) -> Result<(), Error> {
        match expr {
            Expr::Literal { val } => self.resolve_value(val, env),
            Expr::Html(template) => {
                for e in template.embedded_exprs() {
                    self.resolve_expr(e, env)?;
                }
                Ok(())
            }
            Expr::Variable { name } => {
                if env.find(*name).is_none() {
                    return Err(Error::UnboundVariable { name: *name });
                }
                Ok(())
            }
            Expr::Tuple { val } => {
                for e in val {
                    self.resolve_expr(e, env)?;
                }
                Ok(())
            }
            Expr::KeyVal { name: _, value } => self.resolve_expr(value.as_ref(), env),
            Expr::Unop { op: _, expr } => self.resolve_expr(expr.as_ref(), env),
            Expr::Binop {
                op: _,
                expr1,
                expr2,
            } => {
                self.resolve_expr(expr1.as_ref(), env)?;
                self.resolve_expr(expr2.as_ref(), env)
            }
            Expr::If { cond, expr1, expr2 } => {
                self.resolve_expr(cond.as_ref(), env)?;
                self.resolve_expr(expr1.as_ref(), env)?;
                self.resolve_expr(expr2.as_ref(), env)
            }
            Expr::Func {
                params,
                body,
                return_ty: _,
            } => self.resolve_function_body(params, body.as_ref(), env),
            Expr::Call { func, args } => {
                self.resolve_expr(func.as_ref(), env)?;
                for arg in args {
                    self.resolve_expr(arg, env)?;
                }
                Ok(())
            }
            Expr::Action(stmts) => {
                if self.depth >= MAX_SCOPE_DEPTH {
                    return Err(Error::DepthLimit);
                }
                self.depth += 1;
                let mut action_env = Env::new(Some(env));
                let res = self.resolve_action_stmts(stmts, &mut action_env);
                self.depth -= 1;
                res
            }
            Expr::MemberAccess {
                service_name,
                member_name: _,
            } => {
                if env.find(*service_name).is_none() {
                    return Err(Error::UnboundVariable {
                        name: *service_name,
                    });
                }
                Ok(())
            }
            Expr::Select {
                table_name,
                column_names: _,
                where_clause,
            } => {
                if env.find(*table_name).is_none() {
                    return Err(Error::UnboundVariable { name: *table_name });
                }
                self.resolve_expr(where_clause.as_ref(), env)
            }
            Expr::Table { schema: _, records } => {
                for r in records {
                    self.resolve_expr(r, env)?;
                }
                Ok(())
            }
            Expr::Fold {
                table_name,
                column_name: _,
                operation,
                identity,
            } => {
                if env.find(*table_name).is_none() {
                    return Err(Error::UnboundVariable { name: *table_name });
                }
                self.resolve_expr(operation.as_ref(), env)?;
                self.resolve_expr(identity.as_ref(), env)
            }
            Expr::List(exprs) => {
                for expr in exprs {
                    self.resolve_expr(expr, env)?;
                }
                Ok(())
            }
            Expr::Range { start, end } => {
                self.resolve_expr(start.as_ref(), env)?;
                self.resolve_expr(end.as_ref(), env)
            }
        }
    }

    /// Resolves variable names within a value
    ///
    /// Args:
    ///     `val` (`&Value`): The value to resolve
    ///     `env` (`&Env<'_, ()>`): The current environment
    ///
    /// Returns:
    ///     `Result<(), Error>`: Ok if resolution succeeds, or `Error`
    fn resolve_value(&mut self, val: &Value, env: &Env<'_, ()>) -> Result<(), Error> {
        match val {
            Value::Int { val: _ } => Ok(()),
            Value::Bool { val: _ } => Ok(()),
            Value::String { val: _ } => Ok(()),
            Value::Html(_) => Ok(()),
            Value::Closure {
                params,
                body,
                env: _,
                service_name: _,
                return_ty: _,
            } => self.resolve_function_body(params, body.as_ref(), env),
            Value::ActionClosure {
                stmts,
                env: _,
                service_net_id: _,
            } => {
                if self.depth >= MAX_SCOPE_DEPTH {
                    return Err(Error::DepthLimit);
                }
                self.depth += 1;
                let mut action_env = Env::new(Some(env));
                let res = self.resolve_action_stmts(stmts, &mut action_env);
                self.depth -= 1;
                res
            }
            Value::List { vals } => {
                for val in vals {
                    self.resolve_value(val, env)?;
                }
                Ok(())
            }
            Value::Range { start: _, end: _ } => Ok(()),
        }
    }

    /// Resolves the body of a function or closure within a new scope
    ///
    /// Args:
    ///     `params` (`&[Param]`): The function parameters
    ///     `body` (`&Expr`): The body expression to resolve
    ///     `env` (`&Env<'_, ()>`): The parent environment
    ///
    /// Returns:
    ///     `Result<(), Error>`: Ok if resolution succeeds, or `Error`
    fn resolve_function_body(
        &mut self,
        params: &[Param],
        body: &Expr,
        env: &Env<'_, ()>,
    ) -> Result<(), Error> {
        if self.depth >= MAX_SCOPE_DEPTH {
            return Err(Error::DepthLimit);
        }
        self.depth += 1;

        let mut inner_env = Env::new(Some(env));
        for param in params {
            inner_env.bind(param.name, ());
        }

        let res = self.resolve_expr(body, &inner_env);
        self.depth -= 1;
        res
    }
}

/// The public entry point to resolve name bindings in a program
///
/// Args:
///     `stmts` (`&[Stmt]`): The statements of the program
///
/// Returns:
///     `Result<(), Error>`: Ok if all symbols are bound correctly
pub fn resolve(stmts: &[Stmt]) -> Result<(), Error> {
    let mut resolver = Resolver::new();
    let mut root_env = Env::new(None);
    resolver.resolve_program(stmts, &mut root_env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::ast::{ActionStmt, Decl, Expr, Stmt, Value};
    use crate::runtime::interner::Interner;
    use crate::runtime::tt::Param;

    /// Verify that service-level hoisting allows out-of-order decls
    #[test]
    fn test_unit_service_hoisting_semantics() {
        let mut interner = Interner::new();
        let s = interner.insert("s");
        let x = interner.insert("x");
        let y = interner.insert("y");

        // def y = (x + 1);
        let decl_y = Decl::DefDecl {
            name: y,
            ty: None,
            val: Expr::Binop {
                op: crate::runtime::ast::BinOp::Add,
                expr1: Box::new(Expr::Variable { name: x }),
                expr2: Box::new(Expr::Literal {
                    val: Value::Int { val: 1 },
                }),
            },
            is_pub: true,
        };

        // var x = 5;
        let decl_x = Decl::VarDecl {
            name: x,
            ty: None,
            val: Expr::Literal {
                val: Value::Int { val: 5 },
            },
        };

        let stmt = Stmt::Service {
            name: s,
            decls: vec![decl_y, decl_x],
        };

        let mut env = Env::new(None);
        let mut resolver = Resolver::new();
        assert!(resolver.resolve_stmt(&stmt, &mut env).is_ok());

        // Verify that x and y are now bound in the parent scope
        assert!(env.find(s).is_some());
    }

    /// Verify sequential block scoping and declaration-before-use
    #[test]
    fn test_unit_sequential_lexical_scoping() {
        let mut interner = Interner::new();
        let s = interner.insert("s");
        let x = interner.insert("x");
        let local_x = interner.insert("x");

        // Service scope has x
        let decl_x = Decl::VarDecl {
            name: x,
            ty: None,
            val: Expr::Literal {
                val: Value::Int { val: 5 },
            },
        };

        // Action block:
        // do x;
        // let x = 10;
        // do x;
        let action_expr = Expr::Action(vec![
            ActionStmt::Expr(Expr::Variable { name: local_x }),
            ActionStmt::Let {
                name: local_x,
                ty: None,
                expr: Expr::Literal {
                    val: Value::Int { val: 10 },
                },
            },
            ActionStmt::Expr(Expr::Variable { name: local_x }),
        ]);

        let decl_action = Decl::DefDecl {
            name: interner.insert("act"),
            ty: None,
            val: action_expr,
            is_pub: true,
        };

        let stmt = Stmt::Service {
            name: s,
            decls: vec![decl_x, decl_action],
        };

        let mut env = Env::new(None);
        let mut resolver = Resolver::new();
        assert!(resolver.resolve_stmt(&stmt, &mut env).is_ok());

        // Test sequential failure (no service variable x exists)
        // action { do x; let x = 10; }
        // The first 'do x' must fail because x is not yet declared
        let bad_action = Expr::Action(vec![
            ActionStmt::Expr(Expr::Variable { name: local_x }),
            ActionStmt::Let {
                name: local_x,
                ty: None,
                expr: Expr::Literal {
                    val: Value::Int { val: 10 },
                },
            },
        ]);

        let bad_decl = Decl::DefDecl {
            name: interner.insert("act2"),
            ty: None,
            val: bad_action,
            is_pub: true,
        };

        let bad_stmt = Stmt::Service {
            name: s,
            decls: vec![bad_decl],
        };

        let mut bad_env = Env::new(None);
        let res = resolver.resolve_stmt(&bad_stmt, &mut bad_env);
        assert_eq!(res, Err(Error::UnboundVariable { name: local_x }));
    }

    /// Verify lexical isolation between sibling action blocks
    #[test]
    fn test_unit_scope_isolation_and_nesting() {
        let mut interner = Interner::new();
        let x = interner.insert("x");

        // Sibling blocks:
        // block1: action { let x = 5; }
        // block2: action { do x; }
        // block2 must fail because block1's let binding does not leak
        let block1 = ActionStmt::Expr(Expr::Action(vec![ActionStmt::Let {
            name: x,
            ty: None,
            expr: Expr::Literal {
                val: Value::Int { val: 5 },
            },
        }]));

        let block2 = ActionStmt::Expr(Expr::Action(vec![ActionStmt::Expr(Expr::Variable {
            name: x,
        })]));

        let decl = Decl::DefDecl {
            name: interner.insert("d"),
            ty: None,
            val: Expr::Action(vec![block1, block2]),
            is_pub: true,
        };

        let stmt = Stmt::Service {
            name: interner.insert("s"),
            decls: vec![decl],
        };

        let mut env = Env::new(None);
        let mut resolver = Resolver::new();
        let res = resolver.resolve_stmt(&stmt, &mut env);
        assert_eq!(res, Err(Error::UnboundVariable { name: x }));
    }

    /// Verify nested function parameter binding and scope capture
    #[test]
    fn test_unit_closure_variable_capture() {
        let mut interner = Interner::new();
        let a = interner.insert("a");
        let b = interner.insert("b");

        // fn a => fn b => (a + b)
        let body = Expr::Binop {
            op: crate::runtime::ast::BinOp::Add,
            expr1: Box::new(Expr::Variable { name: a }),
            expr2: Box::new(Expr::Variable { name: b }),
        };

        let inner_closure = Expr::Func {
            params: vec![Param { name: b, ty: None }],
            body: Box::new(body),
            return_ty: None,
        };

        let outer_closure = Expr::Func {
            params: vec![Param { name: a, ty: None }],
            body: Box::new(inner_closure),
            return_ty: None,
        };

        let env = Env::new(None);
        let mut resolver = Resolver::new();
        assert!(resolver.resolve_expr(&outer_closure, &env).is_ok());

        // Verify parameter escaping triggers error:
        // fn a => (fn b => b) + b
        // The last 'b' should fail as b has escaped its lexical closure
        let escaped_body = Expr::Binop {
            op: crate::runtime::ast::BinOp::Add,
            expr1: Box::new(Expr::Func {
                params: vec![Param { name: b, ty: None }],
                body: Box::new(Expr::Variable { name: b }),
                return_ty: None,
            }),
            expr2: Box::new(Expr::Variable { name: b }),
        };

        let escaped_closure = Expr::Func {
            params: vec![Param { name: a, ty: None }],
            body: Box::new(escaped_body),
            return_ty: None,
        };

        let res = resolver.resolve_expr(&escaped_closure, &env);
        assert_eq!(res, Err(Error::UnboundVariable { name: b }));
    }

    /// Verify scope depth limit increases and decreases correctly
    #[test]
    fn test_unit_depth_limit_tracking() {
        let env = Env::new(None);
        let mut resolver = Resolver::new();

        // 1. Sibling blocks do not accumulate depth
        // We create a tuple containing two sibling action chains,
        // each nested 100 levels deep.
        // If depth was not decremented on exit, this would cross 128.
        let mut nested_a = Expr::Literal {
            val: Value::Int { val: 0 },
        };
        let mut nested_b = Expr::Literal {
            val: Value::Int { val: 0 },
        };
        for _ in 0..100 {
            nested_a = Expr::Action(vec![ActionStmt::Expr(nested_a)]);
            nested_b = Expr::Action(vec![ActionStmt::Expr(nested_b)]);
        }

        let sibling_tuple = Expr::Tuple {
            val: vec![nested_a, nested_b],
        };

        assert!(resolver.resolve_expr(&sibling_tuple, &env).is_ok());
        assert_eq!(resolver.depth, 0);

        // 2. Nested block exceeding 128 depth triggers limit
        let mut deep_expr = Expr::Literal {
            val: Value::Int { val: 0 },
        };
        for _ in 0..130 {
            deep_expr = Expr::Action(vec![ActionStmt::Expr(deep_expr)]);
        }

        assert_eq!(
            resolver.resolve_expr(&deep_expr, &env),
            Err(Error::DepthLimit)
        );
    }

    /// Verify update statement declarations hoist internally
    #[test]
    fn test_unit_update_block_scope_resolution() {
        let mut interner = Interner::new();
        let s = interner.insert("s");
        let x = interner.insert("x");
        let y = interner.insert("y");

        let s_stmt = Stmt::Service {
            name: s,
            decls: vec![],
        };

        // update s { def y = x; var x = 5; }
        // x must hoist within the update block
        let update_stmt = Stmt::Update {
            service_name: s,
            decls: vec![
                Decl::DefDecl {
                    name: y,
                    ty: None,
                    val: Expr::Variable { name: x },
                    is_pub: false,
                },
                Decl::VarDecl {
                    name: x,
                    ty: None,
                    val: Expr::Literal {
                        val: Value::Int { val: 5 },
                    },
                },
            ],
        };

        let mut env = Env::new(None);
        let mut resolver = Resolver::new();
        assert!(resolver.resolve_stmt(&s_stmt, &mut env).is_ok());
        assert!(resolver.resolve_stmt(&update_stmt, &mut env).is_ok());
    }

    /// Verify that resolver depth state is recovered on early errors
    #[test]
    fn test_unit_depth_limit_recovery_on_error() {
        let mut interner = Interner::new();
        let z = interner.insert("z");
        let env = Env::new(None);
        let mut resolver = Resolver::new();

        // A nested action block containing an unbound variable 'z'
        // nested 5 levels deep
        let mut bad_expr = Expr::Variable { name: z };
        for _ in 0..5 {
            bad_expr = Expr::Action(vec![ActionStmt::Expr(bad_expr)]);
        }

        // Resolving should fail because z is unbound
        let res = resolver.resolve_expr(&bad_expr, &env);
        assert_eq!(res, Err(Error::UnboundVariable { name: z }));

        // Ensure that the depth was unwound and returned to 0
        assert_eq!(resolver.depth, 0);
    }

    /// Verify that `@test` blocks can resolve variables
    /// in the target service
    #[test]
    fn test_unit_test_block_member_resolution() {
        let mut interner = Interner::new();
        let s = interner.insert("s");
        let x = interner.insert("x");

        // Representing `service s { var x = 5; }`
        let s_stmt = Stmt::Service {
            name: s,
            decls: vec![Decl::VarDecl {
                name: x,
                ty: None,
                val: Expr::Literal {
                    val: Value::Int { val: 5 },
                },
            }],
        };

        // Representing `@test(s) { x; }`
        let test_stmt = Stmt::Test {
            service_name: s,
            stmts: vec![ActionStmt::Expr(Expr::Variable { name: x })],
        };

        let mut env = Env::new(None);
        let mut resolver = Resolver::new();
        let program = vec![s_stmt, test_stmt];

        assert!(resolver.resolve_program(&program, &mut env).is_ok());
    }

    /// Verify `@test` blocks can resolve services that
    /// are defined after them in the program statements
    #[test]
    fn test_unit_test_block_hoisting() {
        let mut interner = Interner::new();
        let s = interner.insert("s");
        let x = interner.insert("x");

        // Representing `@test(s) { x; }`
        let test_stmt = Stmt::Test {
            service_name: s,
            stmts: vec![ActionStmt::Expr(Expr::Variable { name: x })],
        };

        // Representing `service s { var x = 5; }`
        let s_stmt = Stmt::Service {
            name: s,
            decls: vec![Decl::VarDecl {
                name: x,
                ty: None,
                val: Expr::Literal {
                    val: Value::Int { val: 5 },
                },
            }],
        };

        let mut env = Env::new(None);
        let mut resolver = Resolver::new();
        let program = vec![test_stmt, s_stmt];

        assert!(resolver.resolve_program(&program, &mut env).is_ok());
    }

    /// Verify testing an imported service yields an
    /// `ImportResolutionUnimplemented` error
    #[test]
    fn test_unit_test_block_imported_unsupported() {
        let mut interner = Interner::new();
        let s = interner.insert("s");
        let x = interner.insert("x");

        // Representing `@test(s) { x; }`
        let test_stmt = Stmt::Test {
            service_name: s,
            stmts: vec![ActionStmt::Expr(Expr::Variable { name: x })],
        };

        let mut env = Env::new(None);
        // Bind service in `env` (simulating import)
        env.bind(s, ());

        let mut resolver = Resolver::new();
        let program = vec![test_stmt];

        let result = resolver.resolve_program(&program, &mut env);
        assert!(result.is_err());
        match result.unwrap_err() {
            Error::ImportResolutionUnimplemented => {}
            Error::UnboundVariable { .. } | Error::DepthLimit => {
                panic!("Expected ImportResolutionUnimplemented error");
            }
        }
    }

    /// Verify testing an undefined service yields an
    /// `UnboundVariable` error
    #[test]
    fn test_unit_test_block_undefined_service() {
        let mut interner = Interner::new();
        let s = interner.insert("s");
        let x = interner.insert("x");

        // Representing `@test(s) { x; }`
        let test_stmt = Stmt::Test {
            service_name: s,
            stmts: vec![ActionStmt::Expr(Expr::Variable { name: x })],
        };

        let mut env = Env::new(None);
        let mut resolver = Resolver::new();
        let program = vec![test_stmt];

        let result = resolver.resolve_program(&program, &mut env);
        assert!(result.is_err());
        match result.unwrap_err() {
            Error::UnboundVariable { name } => {
                assert_eq!(name, s);
            }
            Error::ImportResolutionUnimplemented | Error::DepthLimit => {
                panic!("Expected UnboundVariable error");
            }
        }
    }
}
