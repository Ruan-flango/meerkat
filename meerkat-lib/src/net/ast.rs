//! Network representation for `AST` elements
//!
//! This module defines the serialized equivalents of the runtime `AST`
//! types, substituting `Symbol` identifiers with raw `String` names

use crate::net::ServiceNetId;
use serde::{Deserialize, Serialize};

/// Network representation of a field definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetField {
    pub name: String,
    pub ty: NetDataType,
}

/// Network representation of an action statement
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NetActionStmt {
    /// Bind a value to a local name using `let`
    Let { name: String, expr: NetExpr },
    /// A standalone expression statement
    Expr(NetExpr),
    /// A `do` statement to evaluate an expression for side effects
    Do(NetExpr),
    /// An `assert` statement to check invariants
    Assert(NetExpr),
    /// Re-assign a value to an existing variable
    Assign { name: String, expr: NetExpr },
    /// Insert a record into a table
    Insert { row: NetExpr, table_name: String },
}

/// Network representation of a value
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NetValue {
    /// A numeric integer value
    Number { val: i32 },
    /// A boolean value
    Bool { val: bool },
    /// A `String` literal value
    String { val: String },
    /// A standard closure value with environment
    Closure {
        params: Vec<String>,
        body: Box<NetExpr>,
        env: Vec<(String, NetValue)>,
        service_name: String,
    },
    /// An action closure value with environment and network ID
    ActionClosure {
        stmts: Vec<NetActionStmt>,
        env: Vec<(String, NetValue)>,
        service_net_id: ServiceNetId,
    },
}

/// Network representation of an expression
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NetExpr {
    /// Literal constant value
    Literal { val: NetValue },
    /// Variable reference
    Variable { name: String },
    /// Tuple construct containing elements
    Tuple { val: Vec<NetExpr> },
    /// Key-value attribute binding
    KeyVal { name: String, value: Box<NetExpr> },
    /// Unary operator application
    Unop { op: NetUnOp, expr: Box<NetExpr> },
    /// Binary operator application
    Binop {
        op: NetBinOp,
        expr1: Box<NetExpr>,
        expr2: Box<NetExpr>,
    },
    /// Conditional branching expression
    If {
        cond: Box<NetExpr>,
        expr1: Box<NetExpr>,
        expr2: Box<NetExpr>,
    },
    /// Anonymous function construct
    Func {
        params: Vec<String>,
        body: Box<NetExpr>,
    },
    /// Function call expression
    Call {
        func: Box<NetExpr>,
        args: Vec<NetExpr>,
    },
    /// Embedded action statement block
    Action(Vec<NetActionStmt>),
    /// Accessing a remote service member
    MemberAccess {
        service_name: String,
        member_name: String,
    },
    /// Data selection query
    Select {
        table_name: String,
        column_names: Vec<String>,
        where_clause: Box<NetExpr>,
    },
    /// Inline table structure
    Table {
        schema: Vec<NetField>,
        records: Vec<NetExpr>,
    },
    /// Fold aggregation construct
    Fold {
        table_name: String,
        column_name: String,
        operation: Box<NetExpr>,
        identity: Box<NetExpr>,
    },
}

/// Network representation of a unary operator
///
/// This enum defines the serialized unary operators mapped from the
/// runtime counterparts for transmission over the network
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum NetUnOp {
    /// Negation operator
    Neg,
    /// Logical negation operator
    Not,
}

/// Network representation of a binary operator
///
/// This enum defines the serialized binary operators mapped from the
/// runtime counterparts for transmission over the network
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum NetBinOp {
    /// Addition operator
    Add,
    /// Subtraction operator
    Sub,
    /// Multiplication operator
    Mul,
    /// Division operator
    Div,
    /// Equality comparison operator
    Eq,
    /// Less-than comparison operator
    Lt,
    /// Greater-than comparison operator
    Gt,
    /// Logical conjunction operator
    And,
    /// Logical disjunction operator
    Or,
}

/// Network representation of a data type
///
/// This enum defines the serialized data types mapped from the
/// runtime counterparts for transmission over the network
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NetDataType {
    /// String data type representation
    String,
    /// Number data type representation
    Number,
    /// Boolean data type representation
    Bool,
}
