//! Network codec for `AST` elements
//!
//! Provides encoding and decoding functions to map between the native
//! `AST` types and the serialized network representation variants

use crate::net::ast::{NetActionStmt, NetBinOp, NetDataType, NetExpr, NetField, NetUnOp, NetValue};
use crate::runtime::ast::{ActionStmt, BinOp, DataType, Expr, Field, UnOp, Value};
use crate::runtime::interner::Interner;

/// Encode a runtime `Value` into a network representation
///
/// Args:
///     val (`&Value`): The runtime `Value` to encode
///     interner (`&Interner`): The `Interner` for symbol lookup
///
/// Returns:
///     `NetValue`: The encoded `NetValue` network representation
pub fn encode_value(val: &Value, interner: &Interner) -> NetValue {
    match val {
        Value::Number { val } => NetValue::Number { val: *val },
        Value::Bool { val } => NetValue::Bool { val: *val },
        Value::String { val } => NetValue::String { val: val.clone() },
        Value::Closure {
            params,
            body,
            env,
            service_name,
        } => {
            let encoded_params = params
                .iter()
                .map(|p| interner.get(*p).to_string())
                .collect();
            let encoded_body = Box::new(encode_expr(body, interner));
            let encoded_env = env
                .iter()
                .map(|(k, v)| (interner.get(*k).to_string(), encode_value(v, interner)))
                .collect();
            let encoded_service = interner.get(*service_name).to_string();
            NetValue::Closure {
                params: encoded_params,
                body: encoded_body,
                env: encoded_env,
                service_name: encoded_service,
            }
        }
        Value::ActionClosure {
            stmts,
            env,
            service_net_id,
        } => {
            let encoded_stmts = stmts
                .iter()
                .map(|s| encode_action_stmt(s, interner))
                .collect();
            let encoded_env = env
                .iter()
                .map(|(k, v)| (interner.get(*k).to_string(), encode_value(v, interner)))
                .collect();
            NetValue::ActionClosure {
                stmts: encoded_stmts,
                env: encoded_env,
                service_net_id: service_net_id.clone(),
            }
        }
    }
}

/// Decode a network `NetValue` representation into a runtime `Value`
///
/// Args:
///     val (`NetValue`): The network `NetValue` to decode
///     interner (`&mut Interner`): The `Interner` for symbol creation
///
/// Returns:
///     `Value`: The decoded runtime `Value`
pub fn decode_value(val: NetValue, interner: &mut Interner) -> Value {
    match val {
        NetValue::Number { val } => Value::Number { val },
        NetValue::Bool { val } => Value::Bool { val },
        NetValue::String { val } => Value::String { val },
        NetValue::Closure {
            params,
            body,
            env,
            service_name,
        } => {
            let decoded_params = params.into_iter().map(|p| interner.insert(&p)).collect();
            let decoded_body = Box::new(decode_expr(*body, interner));
            let decoded_env = env
                .into_iter()
                .map(|(k, v)| (interner.insert(&k), decode_value(v, interner)))
                .collect();
            let decoded_service = interner.insert(&service_name);
            Value::Closure {
                params: decoded_params,
                body: decoded_body,
                env: decoded_env,
                service_name: decoded_service,
            }
        }
        NetValue::ActionClosure {
            stmts,
            env,
            service_net_id,
        } => {
            let decoded_stmts = stmts
                .into_iter()
                .map(|s| decode_action_stmt(s, interner))
                .collect();
            let decoded_env = env
                .into_iter()
                .map(|(k, v)| (interner.insert(&k), decode_value(v, interner)))
                .collect();
            Value::ActionClosure {
                stmts: decoded_stmts,
                env: decoded_env,
                service_net_id,
            }
        }
    }
}

/// Encode a runtime `Expr` into a network representation
///
/// Args:
///     expr (`&Expr`): The runtime `Expr` to encode
///     interner (`&Interner`): The `Interner` for symbol lookup
///
/// Returns:
///     `NetExpr`: The encoded `NetExpr` network representation
pub fn encode_expr(expr: &Expr, interner: &Interner) -> NetExpr {
    match expr {
        Expr::Literal { val } => NetExpr::Literal {
            val: encode_value(val, interner),
        },
        Expr::Variable { name } => NetExpr::Variable {
            name: interner.get(*name).to_string(),
        },
        Expr::Tuple { val } => NetExpr::Tuple {
            val: val.iter().map(|e| encode_expr(e, interner)).collect(),
        },
        Expr::KeyVal { name, value } => NetExpr::KeyVal {
            name: interner.get(*name).to_string(),
            value: Box::new(encode_expr(value, interner)),
        },
        Expr::Unop { op, expr } => NetExpr::Unop {
            op: encode_unop(*op),
            expr: Box::new(encode_expr(expr, interner)),
        },
        Expr::Binop { op, expr1, expr2 } => NetExpr::Binop {
            op: encode_binop(*op),
            expr1: Box::new(encode_expr(expr1, interner)),
            expr2: Box::new(encode_expr(expr2, interner)),
        },
        Expr::If { cond, expr1, expr2 } => NetExpr::If {
            cond: Box::new(encode_expr(cond, interner)),
            expr1: Box::new(encode_expr(expr1, interner)),
            expr2: Box::new(encode_expr(expr2, interner)),
        },
        Expr::Func { params, body } => NetExpr::Func {
            params: params
                .iter()
                .map(|p| interner.get(*p).to_string())
                .collect(),
            body: Box::new(encode_expr(body, interner)),
        },
        Expr::Call { func, args } => NetExpr::Call {
            func: Box::new(encode_expr(func, interner)),
            args: args.iter().map(|e| encode_expr(e, interner)).collect(),
        },
        Expr::Action(stmts) => NetExpr::Action(
            stmts
                .iter()
                .map(|s| encode_action_stmt(s, interner))
                .collect(),
        ),
        Expr::MemberAccess {
            service_name,
            member_name,
        } => NetExpr::MemberAccess {
            service_name: interner.get(*service_name).to_string(),
            member_name: interner.get(*member_name).to_string(),
        },
        Expr::Select {
            table_name,
            column_names,
            where_clause,
        } => NetExpr::Select {
            table_name: interner.get(*table_name).to_string(),
            column_names: column_names
                .iter()
                .map(|c| interner.get(*c).to_string())
                .collect(),
            where_clause: Box::new(encode_expr(where_clause, interner)),
        },
        Expr::Table { schema, records } => NetExpr::Table {
            schema: schema.iter().map(|f| encode_field(f, interner)).collect(),
            records: records.iter().map(|r| encode_expr(r, interner)).collect(),
        },
        Expr::Fold {
            table_name,
            column_name,
            operation,
            identity,
        } => NetExpr::Fold {
            table_name: interner.get(*table_name).to_string(),
            column_name: interner.get(*column_name).to_string(),
            operation: Box::new(encode_expr(operation, interner)),
            identity: Box::new(encode_expr(identity, interner)),
        },
    }
}

/// Decode a network `NetExpr` representation into a runtime `Expr`
///
/// Args:
///     expr (`NetExpr`): The network `NetExpr` to decode
///     interner (`&mut Interner`): The `Interner` for symbol creation
///
/// Returns:
///     `Expr`: The decoded runtime `Expr`
pub fn decode_expr(expr: NetExpr, interner: &mut Interner) -> Expr {
    match expr {
        NetExpr::Literal { val } => Expr::Literal {
            val: decode_value(val, interner),
        },
        NetExpr::Variable { name } => Expr::Variable {
            name: interner.insert(&name),
        },
        NetExpr::Tuple { val } => Expr::Tuple {
            val: val.into_iter().map(|e| decode_expr(e, interner)).collect(),
        },
        NetExpr::KeyVal { name, value } => Expr::KeyVal {
            name: interner.insert(&name),
            value: Box::new(decode_expr(*value, interner)),
        },
        NetExpr::Unop { op, expr } => Expr::Unop {
            op: decode_unop(op),
            expr: Box::new(decode_expr(*expr, interner)),
        },
        NetExpr::Binop { op, expr1, expr2 } => Expr::Binop {
            op: decode_binop(op),
            expr1: Box::new(decode_expr(*expr1, interner)),
            expr2: Box::new(decode_expr(*expr2, interner)),
        },
        NetExpr::If { cond, expr1, expr2 } => Expr::If {
            cond: Box::new(decode_expr(*cond, interner)),
            expr1: Box::new(decode_expr(*expr1, interner)),
            expr2: Box::new(decode_expr(*expr2, interner)),
        },
        NetExpr::Func { params, body } => Expr::Func {
            params: params.into_iter().map(|p| interner.insert(&p)).collect(),
            body: Box::new(decode_expr(*body, interner)),
        },
        NetExpr::Call { func, args } => Expr::Call {
            func: Box::new(decode_expr(*func, interner)),
            args: args.into_iter().map(|e| decode_expr(e, interner)).collect(),
        },
        NetExpr::Action(stmts) => Expr::Action(
            stmts
                .into_iter()
                .map(|s| decode_action_stmt(s, interner))
                .collect(),
        ),
        NetExpr::MemberAccess {
            service_name,
            member_name,
        } => Expr::MemberAccess {
            service_name: interner.insert(&service_name),
            member_name: interner.insert(&member_name),
        },
        NetExpr::Select {
            table_name,
            column_names,
            where_clause,
        } => Expr::Select {
            table_name: interner.insert(&table_name),
            column_names: column_names
                .into_iter()
                .map(|c| interner.insert(&c))
                .collect(),
            where_clause: Box::new(decode_expr(*where_clause, interner)),
        },
        NetExpr::Table { schema, records } => Expr::Table {
            schema: schema
                .into_iter()
                .map(|f| decode_field(f, interner))
                .collect(),
            records: records
                .into_iter()
                .map(|r| decode_expr(r, interner))
                .collect(),
        },
        NetExpr::Fold {
            table_name,
            column_name,
            operation,
            identity,
        } => Expr::Fold {
            table_name: interner.insert(&table_name),
            column_name: interner.insert(&column_name),
            operation: Box::new(decode_expr(*operation, interner)),
            identity: Box::new(decode_expr(*identity, interner)),
        },
    }
}

/// Encode a runtime `ActionStmt` into a network representation
///
/// Args:
///     stmt (`&ActionStmt`): The runtime `ActionStmt` to encode
///     interner (`&Interner`): The `Interner` for symbol lookup
///
/// Returns:
///     `NetActionStmt`: The encoded `NetActionStmt` network representation
pub fn encode_action_stmt(stmt: &ActionStmt, interner: &Interner) -> NetActionStmt {
    match stmt {
        ActionStmt::Let { name, expr } => NetActionStmt::Let {
            name: interner.get(*name).to_string(),
            expr: encode_expr(expr, interner),
        },
        ActionStmt::Expr(expr) => NetActionStmt::Expr(encode_expr(expr, interner)),
        ActionStmt::Do(expr) => NetActionStmt::Do(encode_expr(expr, interner)),
        ActionStmt::Assert(expr) => NetActionStmt::Assert(encode_expr(expr, interner)),
        ActionStmt::Assign { name, expr } => NetActionStmt::Assign {
            name: interner.get(*name).to_string(),
            expr: encode_expr(expr, interner),
        },
        ActionStmt::Insert { row, table_name } => NetActionStmt::Insert {
            row: encode_expr(row, interner),
            table_name: interner.get(*table_name).to_string(),
        },
    }
}

/// Decode a network `NetActionStmt` into a runtime `ActionStmt`
///
/// Args:
///     stmt (`NetActionStmt`): The network `NetActionStmt` to decode
///     interner (`&mut Interner`): The `Interner` for symbol creation
///
/// Returns:
///     `ActionStmt`: The decoded runtime `ActionStmt`
pub fn decode_action_stmt(stmt: NetActionStmt, interner: &mut Interner) -> ActionStmt {
    match stmt {
        NetActionStmt::Let { name, expr } => ActionStmt::Let {
            name: interner.insert(&name),
            expr: decode_expr(expr, interner),
        },
        NetActionStmt::Expr(expr) => ActionStmt::Expr(decode_expr(expr, interner)),
        NetActionStmt::Do(expr) => ActionStmt::Do(decode_expr(expr, interner)),
        NetActionStmt::Assert(expr) => ActionStmt::Assert(decode_expr(expr, interner)),
        NetActionStmt::Assign { name, expr } => ActionStmt::Assign {
            name: interner.insert(&name),
            expr: decode_expr(expr, interner),
        },
        NetActionStmt::Insert { row, table_name } => ActionStmt::Insert {
            row: decode_expr(row, interner),
            table_name: interner.insert(&table_name),
        },
    }
}

/// Encode a runtime `Field` into a network representation
///
/// Args:
///     field (`&Field`): The runtime `Field` to encode
///     interner (`&Interner`): The `Interner` for symbol lookup
///
/// Returns:
///     `NetField`: The encoded `NetField` network representation
pub fn encode_field(field: &Field, interner: &Interner) -> NetField {
    NetField {
        name: interner.get(field.name).to_string(),
        ty: encode_datatype(&field.ty),
    }
}

/// Decode a network `NetField` representation into a runtime `Field`
///
/// Args:
///     field (`NetField`): The network `NetField` to decode
///     interner (`&mut Interner`): The `Interner` for symbol creation
///
/// Returns:
///     `Field`: The decoded runtime `Field`
pub fn decode_field(field: NetField, interner: &mut Interner) -> Field {
    Field {
        name: interner.insert(&field.name),
        ty: decode_datatype(field.ty),
    }
}

/// Encode a runtime `UnOp` into its network equivalent
///
/// Args:
///     op (`UnOp`): The runtime operator to encode
///
/// Returns:
///     `NetUnOp`: The encoded network operator representation
pub fn encode_unop(op: UnOp) -> NetUnOp {
    match op {
        UnOp::Neg => NetUnOp::Neg,
        UnOp::Not => NetUnOp::Not,
    }
}

/// Decode a network `NetUnOp` into its runtime equivalent
///
/// Args:
///     op (`NetUnOp`): The network operator to decode
///
/// Returns:
///     `UnOp`: The decoded runtime operator representation
pub fn decode_unop(op: NetUnOp) -> UnOp {
    match op {
        NetUnOp::Neg => UnOp::Neg,
        NetUnOp::Not => UnOp::Not,
    }
}

/// Encode a runtime `BinOp` into its network equivalent
///
/// Args:
///     op (`BinOp`): The runtime operator to encode
///
/// Returns:
///     `NetBinOp`: The encoded network operator representation
pub fn encode_binop(op: BinOp) -> NetBinOp {
    match op {
        BinOp::Add => NetBinOp::Add,
        BinOp::Sub => NetBinOp::Sub,
        BinOp::Mul => NetBinOp::Mul,
        BinOp::Div => NetBinOp::Div,
        BinOp::Eq => NetBinOp::Eq,
        BinOp::Lt => NetBinOp::Lt,
        BinOp::Gt => NetBinOp::Gt,
        BinOp::And => NetBinOp::And,
        BinOp::Or => NetBinOp::Or,
    }
}

/// Decode a network `NetBinOp` into its runtime equivalent
///
/// Args:
///     op (`NetBinOp`): The network operator to decode
///
/// Returns:
///     `BinOp`: The decoded runtime operator representation
pub fn decode_binop(op: NetBinOp) -> BinOp {
    match op {
        NetBinOp::Add => BinOp::Add,
        NetBinOp::Sub => BinOp::Sub,
        NetBinOp::Mul => BinOp::Mul,
        NetBinOp::Div => BinOp::Div,
        NetBinOp::Eq => BinOp::Eq,
        NetBinOp::Lt => BinOp::Lt,
        NetBinOp::Gt => BinOp::Gt,
        NetBinOp::And => BinOp::And,
        NetBinOp::Or => BinOp::Or,
    }
}

/// Encode a runtime `DataType` into its network equivalent
///
/// Args:
///     t (`&DataType`): The runtime data type to encode
///
/// Returns:
///     `NetDataType`: The encoded network data type representation
pub fn encode_datatype(t: &DataType) -> NetDataType {
    match t {
        DataType::String => NetDataType::String,
        DataType::Number => NetDataType::Number,
        DataType::Bool => NetDataType::Bool,
    }
}

/// Decode a network `NetDataType` into its runtime equivalent
///
/// Args:
///     t (`NetDataType`): The network data type to decode
///
/// Returns:
///     `DataType`: The decoded runtime data type representation
pub fn decode_datatype(t: NetDataType) -> DataType {
    match t {
        NetDataType::String => DataType::String,
        NetDataType::Number => DataType::Number,
        NetDataType::Bool => DataType::Bool,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::ServiceNetId;

    /// Verify round-trip encoding, serialization, deserialization, and decoding of `AST` types
    #[test]
    fn test_value_codec_roundtrip() {
        let mut interner_orig = Interner::new();
        let service_net_id = ServiceNetId::new("test_service");

        let var_x = interner_orig.insert("x");
        let tbl_t = interner_orig.insert("t");

        let stmt1 = ActionStmt::Let {
            name: var_x,
            expr: Expr::Literal {
                val: Value::Number { val: 42 },
            },
        };
        let stmt2 = ActionStmt::Insert {
            row: Expr::Variable { name: var_x },
            table_name: tbl_t,
        };

        let env_var = interner_orig.insert("y");
        let env = vec![(env_var, Value::Bool { val: true })];

        let original_value = Value::ActionClosure {
            stmts: vec![stmt1, stmt2],
            env,
            service_net_id,
        };

        let orig_str = format!("{}", original_value);

        let encoded = encode_value(&original_value, &interner_orig);

        let json_str = serde_json::to_string(&encoded).unwrap();
        let decoded_net_val: NetValue = serde_json::from_str(&json_str).unwrap();

        let mut interner_new = Interner::new();
        let decoded_value = decode_value(decoded_net_val, &mut interner_new);

        let new_str = format!("{}", decoded_value);

        assert_eq!(orig_str, new_str);
    }

    /// Verify round-trip encoding and decoding for
    /// Value::String and Value::Closure
    #[test]
    fn test_value_codec_exhaustive() {
        let mut interner_orig = Interner::new();
        let param_name = interner_orig.insert("x");
        let body = Expr::Literal {
            val: Value::String {
                val: "hello".to_string(),
            },
        };
        let env_key = interner_orig.insert("y");
        let env_val = Value::Number { val: 123 };
        let service = interner_orig.insert("my_service");

        let original_value = Value::Closure {
            params: vec![param_name],
            body: Box::new(body),
            env: vec![(env_key, env_val)],
            service_name: service,
        };

        let encoded = encode_value(&original_value, &interner_orig);
        let mut interner_new = Interner::new();
        let decoded = decode_value(encoded, &mut interner_new);

        assert_eq!(format!("{}", original_value), format!("{}", decoded));
    }

    /// Verify round-trip encoding and decoding for Tuple,
    /// KeyVal, Unop, Binop, and If expressions
    #[test]
    fn test_expr_codec_exhaustive_1() {
        let run_expr_test = |expr: &Expr, interner_orig: &Interner| {
            let encoded = encode_expr(expr, interner_orig);
            let mut interner_new = Interner::new();
            let decoded = decode_expr(encoded, &mut interner_new);
            assert_eq!(format!("{}", expr), format!("{}", decoded));
        };

        // 1. Tuple
        let interner = Interner::new();
        let tuple_expr = Expr::Tuple {
            val: vec![
                Expr::Literal {
                    val: Value::Number { val: 1 },
                },
                Expr::Literal {
                    val: Value::Number { val: 2 },
                },
            ],
        };
        run_expr_test(&tuple_expr, &interner);

        // 2. KeyVal
        let mut interner = Interner::new();
        let name_kv = interner.insert("kv_name");
        let key_val_expr = Expr::KeyVal {
            name: name_kv,
            value: Box::new(Expr::Literal {
                val: Value::Number { val: 3 },
            }),
        };
        run_expr_test(&key_val_expr, &interner);

        // 3. Unop (Neg, Not)
        for op in &[UnOp::Neg, UnOp::Not] {
            let interner = Interner::new();
            let unop_expr = Expr::Unop {
                op: *op,
                expr: Box::new(Expr::Literal {
                    val: Value::Bool { val: true },
                }),
            };
            run_expr_test(&unop_expr, &interner);
        }

        // 4. Binop
        let binops = &[
            BinOp::Add,
            BinOp::Sub,
            BinOp::Mul,
            BinOp::Div,
            BinOp::Eq,
            BinOp::Lt,
            BinOp::Gt,
            BinOp::And,
            BinOp::Or,
        ];
        for op in binops {
            let interner = Interner::new();
            let binop_expr = Expr::Binop {
                op: *op,
                expr1: Box::new(Expr::Literal {
                    val: Value::Number { val: 5 },
                }),
                expr2: Box::new(Expr::Literal {
                    val: Value::Number { val: 6 },
                }),
            };
            run_expr_test(&binop_expr, &interner);
        }

        // 5. If
        let interner = Interner::new();
        let if_expr = Expr::If {
            cond: Box::new(Expr::Literal {
                val: Value::Bool { val: true },
            }),
            expr1: Box::new(Expr::Literal {
                val: Value::Number { val: 7 },
            }),
            expr2: Box::new(Expr::Literal {
                val: Value::Number { val: 8 },
            }),
        };
        run_expr_test(&if_expr, &interner);
    }

    /// Verify round-trip encoding and decoding for Func,
    /// Call, Action, and MemberAccess expressions
    #[test]
    fn test_expr_codec_exhaustive_2() {
        let run_expr_test = |expr: &Expr, interner_orig: &Interner| {
            let encoded = encode_expr(expr, interner_orig);
            let mut interner_new = Interner::new();
            let decoded = decode_expr(encoded, &mut interner_new);
            assert_eq!(format!("{}", expr), format!("{}", decoded));
        };

        // 1. Func
        let mut interner = Interner::new();
        let param_name = interner.insert("p");
        let func_expr = Expr::Func {
            params: vec![param_name],
            body: Box::new(Expr::Literal {
                val: Value::Number { val: 9 },
            }),
        };
        run_expr_test(&func_expr, &interner);

        // 2. Call
        let mut interner = Interner::new();
        let param_name = interner.insert("p");
        let func_expr = Expr::Func {
            params: vec![param_name],
            body: Box::new(Expr::Literal {
                val: Value::Number { val: 9 },
            }),
        };
        let call_expr = Expr::Call {
            func: Box::new(func_expr),
            args: vec![Expr::Literal {
                val: Value::Number { val: 10 },
            }],
        };
        run_expr_test(&call_expr, &interner);

        // 3. MemberAccess
        let mut interner = Interner::new();
        let service_name = interner.insert("srv");
        let member_name = interner.insert("mem");
        let member_expr = Expr::MemberAccess {
            service_name,
            member_name,
        };
        run_expr_test(&member_expr, &interner);

        // 4. Action
        let interner = Interner::new();
        let action_expr = Expr::Action(vec![ActionStmt::Do(Expr::Literal {
            val: Value::Number { val: 11 },
        })]);
        run_expr_test(&action_expr, &interner);
    }

    /// Verify round-trip encoding and decoding for Select,
    /// Table, and Fold expressions
    #[test]
    fn test_expr_codec_exhaustive_3() {
        let run_expr_test = |expr: &Expr, interner_orig: &Interner| {
            let encoded = encode_expr(expr, interner_orig);
            let mut interner_new = Interner::new();
            let decoded = decode_expr(encoded, &mut interner_new);
            assert_eq!(format!("{}", expr), format!("{}", decoded));
        };

        // 1. Select
        let mut interner = Interner::new();
        let table_name = interner.insert("tbl");
        let col1 = interner.insert("col1");
        let col2 = interner.insert("col2");
        let select_expr = Expr::Select {
            table_name,
            column_names: vec![col1, col2],
            where_clause: Box::new(Expr::Literal {
                val: Value::Bool { val: true },
            }),
        };
        run_expr_test(&select_expr, &interner);

        // 2. Table
        let mut interner = Interner::new();
        let col1 = interner.insert("col1");
        let col2 = interner.insert("col2");
        let f1 = Field {
            name: col1,
            ty: DataType::String,
        };
        let f2 = Field {
            name: col2,
            ty: DataType::Number,
        };
        let f3 = Field {
            name: col2,
            ty: DataType::Bool,
        };
        let table_expr = Expr::Table {
            schema: vec![f1, f2, f3],
            records: vec![Expr::Literal {
                val: Value::String {
                    val: "abc".to_string(),
                },
            }],
        };
        run_expr_test(&table_expr, &interner);

        // 3. Fold
        let mut interner = Interner::new();
        let table_name = interner.insert("tbl");
        let col1 = interner.insert("col1");
        let fold_expr = Expr::Fold {
            table_name,
            column_name: col1,
            operation: Box::new(Expr::Literal {
                val: Value::Number { val: 42 },
            }),
            identity: Box::new(Expr::Literal {
                val: Value::Number { val: 0 },
            }),
        };
        run_expr_test(&fold_expr, &interner);
    }

    /// Verify round-trip encoding and decoding for Expr,
    /// Do, Assert, and Assign ActionStmts
    #[test]
    fn test_action_stmt_codec_exhaustive() {
        let run_stmt_test = |stmt: &ActionStmt, interner_orig: &Interner| {
            let encoded = encode_action_stmt(stmt, interner_orig);
            let mut interner_new = Interner::new();
            let decoded = decode_action_stmt(encoded, &mut interner_new);
            assert_eq!(format!("{}", stmt), format!("{}", decoded));
        };

        // 1. Expr
        let interner = Interner::new();
        let stmt_expr = ActionStmt::Expr(Expr::Literal {
            val: Value::Number { val: 100 },
        });
        run_stmt_test(&stmt_expr, &interner);

        // 2. Do
        let interner = Interner::new();
        let stmt_do = ActionStmt::Do(Expr::Literal {
            val: Value::Number { val: 200 },
        });
        run_stmt_test(&stmt_do, &interner);

        // 3. Assert
        let interner = Interner::new();
        let stmt_assert = ActionStmt::Assert(Expr::Literal {
            val: Value::Bool { val: true },
        });
        run_stmt_test(&stmt_assert, &interner);

        // 4. Assign
        let mut interner = Interner::new();
        let name_var = interner.insert("v");
        let stmt_assign = ActionStmt::Assign {
            name: name_var,
            expr: Expr::Literal {
                val: Value::Number { val: 300 },
            },
        };
        run_stmt_test(&stmt_assign, &interner);
    }

    /// Verify that deserializing a structurally corrupted JSON
    /// string is rejected safely
    #[test]
    fn test_codec_corrupt_payload_rejection() {
        let malformed_json = "{ \"val\": { \"Closure\": { \"params\": [";
        let res: Result<NetValue, _> = serde_json::from_str(malformed_json);
        assert!(res.is_err() == true);
    }

    /// Verify that type mismatches in JSON are rejected safely
    /// at the boundary
    #[test]
    fn test_codec_type_mismatch_rejection() {
        let mismatched_json = "{ \"Bool\": { \"val\": \"not_a_bool\" } }";
        let res: Result<NetValue, _> = serde_json::from_str(mismatched_json);
        assert!(res.is_err() == true);
    }

    /// Verify that deeply nested AST structures do not crash the
    /// encoder or decoder
    #[test]
    fn test_codec_deeply_nested_structure() {
        let mut expr = Expr::Literal {
            val: Value::Number { val: 0 },
        };
        let mut interner = Interner::new();
        for _ in 0..20 {
            expr = Expr::Binop {
                op: BinOp::Add,
                expr1: Box::new(expr),
                expr2: Box::new(Expr::Literal {
                    val: Value::Number { val: 1 },
                }),
            };
        }

        let encoded = encode_expr(&expr, &interner);
        let json_str = serde_json::to_string(&encoded).unwrap();
        let decoded_net: NetExpr = serde_json::from_str(&json_str).unwrap();
        let decoded = decode_expr(decoded_net, &mut interner);

        assert_eq!(format!("{}", expr), format!("{}", decoded));
    }
}
