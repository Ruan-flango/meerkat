//! AST pretty printer inspired by Rust syntax. This module is primarily
//! invoked by the `--ast` flag for use in testing and development. It
//! supports configurable indentation levels with `INDENTATION` constant

use crate::runtime::ast::{ActionStmt, Decl, Expr, Field, Stmt, Value};

const INDENTATION: usize = 2;

/// Pretty printer for formatting and displaying the abstract syntax tree
pub struct AstPrinter {
    spaces: usize,
}

impl Default for AstPrinter {
    /// Creates a default AstPrinter with two spaces of indentation
    fn default() -> Self {
        Self::new()
    }
}

impl AstPrinter {
    /// Creates a new AstPrinter instance with default indentation
    pub fn new() -> Self {
        Self {
            spaces: INDENTATION,
        }
    }

    /// Creates a new AstPrinter instance with custom indentation level
    pub fn with_spaces(spaces: usize) -> Self {
        Self { spaces }
    }

    /// Prints spaces corresponding to the current indentation level
    fn print_indent(&self, indent: usize) {
        print!("{}", " ".repeat(indent * self.spaces));
    }

    /// Prints a sequence of top-level statements representing a program
    pub fn print_program(&self, stmts: &[Stmt]) {
        for stmt in stmts {
            self.print_stmt(stmt, 0);
        }
    }

    /// Prints a single top-level statement with the specified indentation
    pub fn print_stmt(&self, stmt: &Stmt, indent: usize) {
        self.print_indent(indent);
        match stmt {
            Stmt::ActionStmt(action) => {
                println!("ActionStmt:");
                self.print_action_stmt(action, indent + 1);
            }
            Stmt::Update { service, decls } => {
                println!("Update: {{ service: \"{}\" }}", service);
                for decl in decls {
                    self.print_decl(decl, indent + 1);
                }
            }
            Stmt::Connect { path, addr } => {
                println!("Connect: {{ path: \"{}\", addr: \"{}\" }}", path, addr);
            }
            Stmt::Import { path, service } => {
                println!("Import: {{ path: \"{}\", service: \"{}\" }}", path, service);
            }
            Stmt::Service { name, decls } => {
                println!("Service: {{ name: \"{}\" }}", name);
                for decl in decls {
                    self.print_decl(decl, indent + 1);
                }
            }
            Stmt::Test { service, stmts } => {
                println!("Test: {{ service: \"{}\" }}", service);
                for s in stmts {
                    self.print_action_stmt(s, indent + 1);
                }
            }
            Stmt::Watch { expr } => {
                println!("Watch:");
                self.print_expr(expr, indent + 1);
            }
        }
    }

    /// Prints a declaration statement with the specified indentation
    pub fn print_decl(&self, decl: &Decl, indent: usize) {
        self.print_indent(indent);
        match decl {
            Decl::VarDecl { name, val } => {
                println!("VarDecl: {{ name: \"{}\" }}", name);
                self.print_expr(val, indent + 1);
            }
            Decl::DefDecl { name, val, is_pub } => {
                println!("DefDecl: {{ name: \"{}\", is_pub: {} }}", name, is_pub);
                self.print_expr(val, indent + 1);
            }
            Decl::TableDecl { name, fields } => {
                println!("TableDecl: {{ name: \"{}\" }}", name);
                for field in fields {
                    self.print_field(field, indent + 1);
                }
            }
        }
    }

    /// Prints a record/table field description with the specified indentation
    fn print_field(&self, field: &Field, indent: usize) {
        self.print_indent(indent);
        println!(
            "Field: {{ name: \"{}\", type_: {:?} }}",
            field.name, field.type_
        );
    }

    /// Prints an action statement with the specified indentation
    pub fn print_action_stmt(&self, stmt: &ActionStmt, indent: usize) {
        self.print_indent(indent);
        match stmt {
            ActionStmt::Let { name, expr, .. } => {
                println!("Let: {{ name: \"{}\" }}", name);
                self.print_expr(expr, indent + 1);
            }
            ActionStmt::Expr(expr) => {
                println!("Expr:");
                self.print_expr(expr, indent + 1);
            }
            ActionStmt::Do(expr) => {
                println!("Do:");
                self.print_expr(expr, indent + 1);
            }
            ActionStmt::Assert(expr) => {
                println!("Assert:");
                self.print_expr(expr, indent + 1);
            }
            ActionStmt::Assign { var, expr } => {
                println!("Assign: {{ var: \"{}\" }}", var);
                self.print_expr(expr, indent + 1);
            }
            ActionStmt::Insert { row, table_name } => {
                println!("Insert: {{ table_name: \"{}\" }}", table_name);
                self.print_expr(row, indent + 1);
            }
        }
    }

    /// Prints an expression with the specified indentation
    pub fn print_expr(&self, expr: &Expr, indent: usize) {
        self.print_indent(indent);
        match expr {
            Expr::Literal { val } => {
                println!("Literal:");
                self.print_value(val, indent + 1);
            }
            Expr::Variable { ident, .. } => {
                println!("Variable: {{ ident: \"{}\" }}", ident);
            }
            Expr::Tuple { val } => {
                println!("Tuple:");
                for v in val {
                    self.print_expr(v, indent + 1);
                }
            }
            Expr::KeyVal { key, value } => {
                println!("KeyVal: {{ key: \"{}\" }}", key);
                self.print_expr(value, indent + 1);
            }
            Expr::Unop { op, expr } => {
                println!("Unop: {{ op: {:?} }}", op);
                self.print_expr(expr, indent + 1);
            }
            Expr::Binop { op, expr1, expr2 } => {
                println!("Binop: {{ op: {:?} }}", op);
                self.print_expr(expr1, indent + 1);
                self.print_expr(expr2, indent + 1);
            }
            Expr::If { cond, expr1, expr2 } => {
                println!("If:");
                self.print_expr(cond, indent + 1);
                self.print_expr(expr1, indent + 1);
                self.print_expr(expr2, indent + 1);
            }
            Expr::Func { params, body } => {
                println!("Func: {{ params: {:?} }}", params);
                self.print_expr(body, indent + 1);
            }
            Expr::Call { func, args } => {
                println!("Call:");
                self.print_expr(func, indent + 1);
                for arg in args {
                    self.print_expr(arg, indent + 1);
                }
            }
            Expr::Action(stmts) => {
                println!("Action:");
                for stmt in stmts {
                    self.print_action_stmt(stmt, indent + 1);
                }
            }
            Expr::MemberAccess { service, member } => {
                println!(
                    "MemberAccess: {{ service: \"{}\", member: \"{}\" }}",
                    service, member
                );
            }
            Expr::Select {
                table_name,
                column_names,
                where_clause,
            } => {
                println!(
                    "Select: {{ table_name: \"{}\", column_names: {:?} }}",
                    table_name, column_names
                );
                self.print_expr(where_clause, indent + 1);
            }
            Expr::Table { schema, records } => {
                println!("Table:");
                for field in schema {
                    self.print_field(field, indent + 1);
                }
                for record in records {
                    self.print_expr(record, indent + 1);
                }
            }
            Expr::Fold {
                table_name,
                column_name,
                operation,
                identity,
            } => {
                println!(
                    "Fold: {{ table_name: \"{}\", column_name: \"{}\" }}",
                    table_name, column_name
                );
                self.print_expr(operation, indent + 1);
                self.print_expr(identity, indent + 1);
            }
        }
    }

    /// Prints a runtime value representation with the specified indentation
    pub fn print_value(&self, val: &Value, indent: usize) {
        self.print_indent(indent);
        match val {
            Value::Number { val } => {
                println!("Number: {}", val);
            }
            Value::Bool { val } => {
                println!("Bool: {}", val);
            }
            Value::String { val } => {
                println!("String: \"{}\"", val);
            }
            Value::Closure {
                params,
                body,
                env: _,
                service_name,
            } => {
                println!(
                    "Closure: {{ params: {:?}, service_name: \"{}\" }}",
                    params, service_name
                );
                self.print_expr(body, indent + 1);
            }
            Value::ActionClosure {
                stmts,
                env: _,
                service,
            } => {
                println!("ActionClosure: {{ service: \"{}\" }}", service.0);
                for stmt in stmts {
                    self.print_action_stmt(stmt, indent + 1);
                }
            }
        }
    }
}
