//! Type system representations
//!
//! This module defines the core type representation structures used during parsing,
//! type checking, and translation of type annotations

use crate::runtime::interner::Symbol;

/// Represents a type in the Meerkat language
///
/// This enum models all valid types including primitives, tuples, and function
/// signatures
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Type {
    Int,
    String,
    Bool,
    Unit,
    Tuple(TupleType),
    Func(Box<Type>, Box<Type>),
}

/// A tuple type wrapping a list of types of arity 2 or greater
///
/// This structure enforces the canonical representation invariant
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TupleType(Vec<Type>);

impl TupleType {
    /// Create a new `TupleType` with at least 2 elements
    ///
    /// Args:
    ///     `types` (`Vec<Type>`): The list of types to wrap
    ///
    /// Returns:
    ///     `Result<Self, &'static str>`: The constructed `TupleType`
    ///     if valid
    pub fn new(types: Vec<Type>) -> Result<Self, &'static str> {
        if types.len() < 2 {
            Err("Tuple must contain at least 2 elements")
        } else {
            Ok(Self(types))
        }
    }
}

impl std::ops::Deref for TupleType {
    type Target = [Type];

    /// Dereference to the underlying slice of `Type`
    ///
    /// Returns:
    ///     `&[Type]`: The slice of types
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::fmt::Display for TupleType {
    /// Format the tuple type for display
    ///
    /// Args:
    ///     `f` (`&mut std::fmt::Formatter<'_>`): Formatter
    ///
    /// Returns:
    ///     `std::fmt::Result`: The result of formatting
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "(")?;
        for (i, t) in self.0.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{}", t)?;
        }
        write!(f, ")")
    }
}

/// Represents a function parameter
///
/// This structure holds the name of a parameter and its optional type annotation
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Param {
    pub name: Symbol,
    pub ty: Option<Type>,
}

/// Implement the `Display` trait for the `Type` type
///
/// Provides a human-readable string representation of a type
impl std::fmt::Display for Type {
    /// Format the type for display
    ///
    /// Args:
    ///     `f` (`&mut std::fmt::Formatter<'_>`): The formatter target
    ///
    /// Returns:
    ///     `std::fmt::Result`: The result of the formatting operation
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Type::Int => write!(f, "int"),
            Type::String => write!(f, "string"),
            Type::Bool => write!(f, "bool"),
            Type::Unit => write!(f, "unit"),
            Type::Tuple(ts) => write!(f, "{}", ts),
            Type::Func(t1, t2) => {
                // Determine if the left-hand side is a function type
                // to preserve right-associativity during formatting
                let is_func = match t1.as_ref() {
                    Type::Func(_, _) => true,
                    Type::Int | Type::String | Type::Bool | Type::Unit | Type::Tuple(_) => false,
                };
                if is_func {
                    write!(f, "({}) -> {}", t1, t2)
                } else if matches!(t1.as_ref(), Type::Unit) {
                    write!(f, "() -> {}", t2)
                } else {
                    write!(f, "{} -> {}", t1, t2)
                }
            }
        }
    }
}

/// Implement the `Display` trait for the `Param` type
///
/// Provides a human-readable string representation of a parameter
impl std::fmt::Display for Param {
    /// Format the parameter for display
    ///
    /// Args:
    ///     `f` (`&mut std::fmt::Formatter<'_>`): The formatter target
    ///
    /// Returns:
    ///     `std::fmt::Result`: The result of the formatting operation
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(ty) = &self.ty {
            write!(f, "{}: {}", self.name, ty)
        } else {
            write!(f, "{}", self.name)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that `Type` formats nested function types to
    /// preserve associativity
    #[test]
    fn test_nested_type_formatting() {
        // case 1: (int -> bool) -> string
        let ty1 = Type::Func(
            Box::new(Type::Func(Box::new(Type::Int), Box::new(Type::Bool))),
            Box::new(Type::String),
        );
        assert_eq!(ty1.to_string(), "(int -> bool) -> string");

        // case 2: int -> bool -> string (which is int -> (bool -> string))
        let ty2 = Type::Func(
            Box::new(Type::Int),
            Box::new(Type::Func(Box::new(Type::Bool), Box::new(Type::String))),
        );
        assert_eq!(ty2.to_string(), "int -> bool -> string");

        // case 3: ((int -> string) -> bool) -> unit
        let ty3 = Type::Func(
            Box::new(Type::Func(
                Box::new(Type::Func(Box::new(Type::Int), Box::new(Type::String))),
                Box::new(Type::Bool),
            )),
            Box::new(Type::Unit),
        );
        assert_eq!(ty3.to_string(), "((int -> string) -> bool) -> unit");

        // case 4: (int -> bool) -> (string -> unit)
        let ty4 = Type::Func(
            Box::new(Type::Func(Box::new(Type::Int), Box::new(Type::Bool))),
            Box::new(Type::Func(Box::new(Type::String), Box::new(Type::Unit))),
        );
        assert_eq!(ty4.to_string(), "(int -> bool) -> string -> unit");

        // case 5: () -> int
        let ty5 = Type::Func(Box::new(Type::Unit), Box::new(Type::Int));
        assert_eq!(ty5.to_string(), "() -> int");

        // case 6: (() -> int) -> bool
        let ty6 = Type::Func(
            Box::new(Type::Func(Box::new(Type::Unit), Box::new(Type::Int))),
            Box::new(Type::Bool),
        );
        assert_eq!(ty6.to_string(), "(() -> int) -> bool");

        // case 7: (unit) -> int
        let ty7 = Type::Func(
            Box::new(Type::Tuple(
                TupleType::new(vec![Type::Unit, Type::Unit]).unwrap(),
            )),
            Box::new(Type::Int),
        );
        assert_eq!(ty7.to_string(), "(unit, unit) -> int");
    }

    /// Verify that `TupleType` enforces arity constraints
    /// and supports `Deref`
    #[test]
    fn test_tuple_type_invariants() {
        // Negative cases
        assert!(TupleType::new(vec![]).is_err());
        assert!(TupleType::new(vec![Type::Int]).is_err());

        // Positive cases
        let types = vec![Type::Int, Type::String];
        let tuple_res = TupleType::new(types.clone());
        assert!(tuple_res.is_ok());
        let tuple = tuple_res.unwrap();

        // Test `Deref` slice behavior
        assert_eq!(tuple.len(), 2);
        assert_eq!(tuple[0], Type::Int);
        assert_eq!(tuple[1], Type::String);
    }
}
