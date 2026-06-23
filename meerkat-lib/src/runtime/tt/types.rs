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
    Tuple(Vec<Type>),
    Func(Box<Type>, Box<Type>),
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
            Type::Tuple(ts) => {
                write!(f, "(")?;
                for (i, t) in ts.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", t)?;
                }
                write!(f, ")")
            }
            Type::Func(t1, t2) => {
                // Determine if the left-hand side is a function type
                // to preserve right-associativity during formatting
                let is_func = match &**t1 {
                    Type::Func(_, _) => true,
                    Type::Int | Type::String | Type::Bool | Type::Unit | Type::Tuple(_) => false,
                };
                if is_func {
                    write!(f, "({}) -> {}", t1, t2)
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
    }
}
