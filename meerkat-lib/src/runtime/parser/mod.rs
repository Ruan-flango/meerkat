pub mod lex;

// LALRPOP-generated parser
#[allow(clippy::all)]
#[allow(unused)]
pub mod meerkat {
    include!(concat!(env!("OUT_DIR"), "/runtime/parser/meerkat.rs"));
}

/// Result of attempting to parse a `REPL` input buffer using `ReplParseResult`
pub enum ReplParseResult {
    /// Input parsed successfully into one or more statements
    Complete(Vec<crate::ast::Stmt>),
    /// Input is syntactically incomplete (e.g., an open brace with no matching close)
    ///
    /// The `REPL` should prompt for more input and append it to the buffer
    Incomplete,
    /// Input has a real syntax error that won't be resolved by adding more text
    Error(String),
}

use crate::ast::Stmt;
use crate::runtime::interner::Interner;
use crate::runtime::limits::{MAX_IDENTIFIER_LENGTH, MAX_STRING_LITERAL_LENGTH};
use logos::Logos;

/// Parse a string input into a vector of statements
///
/// Args:
///     `input` (`&str`): The raw string input to parse
///     `interner` (`&mut Interner`): The string interner instance
///
/// Returns:
///     `Result<Vec<Stmt>, String>`: The parsed statements, or an error string
pub fn parse_string(input: &str, interner: &mut Interner) -> Result<Vec<Stmt>, String> {
    let mut lex_stream = Vec::new();
    for (t, span) in lex::Token::lexer(input).spanned() {
        match t {
            lex::Token::Ident(name) if name.len() > MAX_IDENTIFIER_LENGTH => {
                return Err(format!(
                    "Parse error: identifier exceeds maximum length of {} characters",
                    MAX_IDENTIFIER_LENGTH
                ));
            }
            lex::Token::StrLit(val) if val.len() > MAX_STRING_LITERAL_LENGTH => {
                return Err(format!(
                    "Parse error: string literal exceeds maximum length of {} characters",
                    MAX_STRING_LITERAL_LENGTH
                ));
            }
            _ => {}
        }
        lex_stream.push((span.start, t, span.end));
    }

    meerkat::ProgParser::new()
        .parse(input, interner, lex_stream)
        .map_err(|e| format!("Parse error: {:?}", e))
}

/// Parse a file path into a vector of statements
///
/// Args:
///     `filename` (`&str`): The path of the file to parse
///     `interner` (`&mut Interner`): The string interner instance
///
/// Returns:
///     `Result<Vec<Stmt>, String>`: The parsed statements, or an error string
pub fn parse_file(filename: &str, interner: &mut Interner) -> Result<Vec<Stmt>, String> {
    let content =
        std::fs::read_to_string(filename).map_err(|e| format!("Failed to read file: {}", e))?;
    parse_string(&content, interner)
}

/// Try to parse accumulated `REPL` input, distinguishing incomplete input from real errors
///
/// Returns `Incomplete` when the grammar signals `UnrecognizedEof`, meaning the user
/// is mid-statement and the `REPL` should collect more lines before evaluating
///
/// Args:
///     `input` (`&str`): The accumulated REPL input buffer
///     `interner` (`&mut Interner`): The string interner instance
///
/// Returns:
///     `ReplParseResult`: The parsed result status
pub fn parse_repl(input: &str, interner: &mut Interner) -> ReplParseResult {
    use lalrpop_util::ParseError;

    if input.trim().is_empty() {
        return ReplParseResult::Incomplete;
    }

    let mut lex_stream = Vec::new();
    for (t, span) in lex::Token::lexer(input).spanned() {
        match t {
            lex::Token::Ident(name) if name.len() > MAX_IDENTIFIER_LENGTH => {
                return ReplParseResult::Error(format!(
                    "Parse error: identifier exceeds maximum length of {} characters",
                    MAX_IDENTIFIER_LENGTH
                ));
            }
            lex::Token::StrLit(val) if val.len() > MAX_STRING_LITERAL_LENGTH => {
                return ReplParseResult::Error(format!(
                    "Parse error: string literal exceeds maximum length of {} characters",
                    MAX_STRING_LITERAL_LENGTH
                ));
            }
            _ => {}
        }
        lex_stream.push((span.start, t, span.end));
    }

    let parser = meerkat::ProgParser::new();
    match parser.parse(input, interner, lex_stream) {
        Ok(stmts) => match stmts.first() {
            Some(_) => ReplParseResult::Complete(stmts),
            None => ReplParseResult::Incomplete,
        },
        Err(ParseError::UnrecognizedEof { .. }) => ReplParseResult::Incomplete,
        Err(e) => ReplParseResult::Error(format!("{:?}", e)),
    }
}

/// #39: Split a raw html-literal body into parts and parse each `{...}`
/// interpolation as an expression.
///
/// This is the single authoritative place that knows how an html template is
/// decomposed: literal text is copied verbatim, and each brace-delimited
/// interpolation is parsed via the same expression grammar used everywhere
/// else (the `pub Expr` rule), so interpolations are ordinary expressions.
///
/// Braces nest: an interpolation runs from an opening `{` to its matching `}`,
/// counting depth, so an interpolation containing braces is handled. A `{`
/// with no matching `}` is a parse error.
///
/// Args:
///     `raw` (`&str`): The inner html text (between the outer parens)
///     `interner` (`&mut Interner`): The string interner instance
///
/// Returns:
///     `Result<Vec<HtmlPart>, String>`: The parsed template parts, or an error
pub fn parse_html_parts(
    raw: &str,
    interner: &mut Interner,
) -> Result<Vec<crate::ast::HtmlPart>, String> {
    use crate::ast::HtmlPart;

    let mut parts: Vec<HtmlPart> = Vec::new();
    let mut text = String::new();
    let mut chars = raw.char_indices().peekable();

    while let Some((_, c)) = chars.next() {
        if c == '{' {
            // Flush any accumulated literal text.
            if !text.is_empty() {
                parts.push(HtmlPart::Text(std::mem::take(&mut text)));
            }
            // Collect the interpolation source up to the matching `}`.
            let mut depth: usize = 1;
            let mut frag = String::new();
            let mut closed = false;
            for (_, ic) in chars.by_ref() {
                match ic {
                    '{' => {
                        depth += 1;
                        frag.push(ic);
                    }
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            closed = true;
                            break;
                        }
                        frag.push(ic);
                    }
                    _ => frag.push(ic),
                }
            }
            if !closed {
                return Err(
                    "Parse error: unterminated { } interpolation in html literal".to_string(),
                );
            }
            // Parse the fragment as a single expression via the public Expr rule.
            let expr = parse_expr_fragment(frag.trim(), interner)?;
            parts.push(HtmlPart::Expr(Box::new(expr)));
        } else {
            text.push(c);
        }
    }
    if !text.is_empty() {
        parts.push(HtmlPart::Text(text));
    }
    Ok(parts)
}

/// #39: Parse a single interpolation fragment into an `Expr` using the public
/// `Expr` grammar rule, reusing the same lexer and parser as top-level code.
fn parse_expr_fragment(src: &str, interner: &mut Interner) -> Result<crate::ast::Expr, String> {
    let mut lex_stream = Vec::new();
    for (t, span) in lex::Token::lexer(src).spanned() {
        lex_stream.push((span.start, t, span.end));
    }
    meerkat::ExprParser::new()
        .parse(src, interner, lex_stream)
        .map_err(|e| format!("Parse error in html interpolation: {:?}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::interner::Interner;

    /// Verify that parsing an identifier exceeding the limit
    /// returns an error
    #[test]
    fn test_parse_oversized_identifier() {
        let mut interner = Interner::new();
        let long_ident = "a".repeat(MAX_IDENTIFIER_LENGTH + 1);
        let input = format!("let {} = 42;", long_ident);
        let res = parse_string(&input, &mut interner);
        assert!(res.is_err());
        assert!(res
            .unwrap_err()
            .contains("identifier exceeds maximum length"));
    }

    /// Verify that parsing a string literal exceeding the limit
    /// returns an error
    #[test]
    fn test_parse_oversized_string_literal() {
        let mut interner = Interner::new();
        let long_str = "a".repeat(MAX_STRING_LITERAL_LENGTH + 1);
        let input = format!("let x = \"{}\";", long_str);
        let res = parse_string(&input, &mut interner);
        assert!(res.is_err());
        assert!(res
            .unwrap_err()
            .contains("string literal exceeds maximum length"));
    }
    /// Verify that parsing an assertion captures the correct
    /// raw string
    #[test]
    fn test_parse_assert_captures_string() {
        use crate::ast::{ActionStmt, Stmt};

        let mut interner = Interner::new();
        let input = "assert (x == 5);";
        let res = parse_string(input, &mut interner);
        assert!(res.is_ok());
        let ast = res.unwrap();
        assert_eq!(ast.len(), 1);
        match &ast[0] {
            Stmt::ActionStmt(ActionStmt::Assert(_, text)) => {
                assert_eq!(text, "x == 5");
            }
            _ => panic!("Expected ActionStmt::Assert"),
        }
    }

    /// Verify that parsing an assertion exceeding length
    /// limit fails
    #[test]
    fn test_parse_oversized_assert() {
        let mut interner = Interner::new();
        let limit = MAX_STRING_LITERAL_LENGTH;
        let long_expr = "1+".repeat((limit / 2) + 1) + "1";
        let input = format!("assert ({});", long_expr);
        let res = parse_string(&input, &mut interner);
        assert!(res.is_err());
        assert!(res
            .unwrap_err()
            .contains("Assertion text exceeds maximum length"));
    }

    /// Verify that parsing an `assert` exceeding the length limit
    /// returns a `ParseError::User` error variant
    #[test]
    fn test_parse_oversized_assert_error_type() {
        use lalrpop_util::ParseError;
        let mut interner = Interner::new();
        let limit = MAX_STRING_LITERAL_LENGTH;
        let half_limit = limit / 2;
        let repeat_count = half_limit + 1;
        let repeated = "1+".repeat(repeat_count);
        let long_expr = format!("{}1", repeated);
        let input = format!("assert ({});", long_expr);
        let mut lex_stream = Vec::new();
        for (t, span) in lex::Token::lexer(&input).spanned() {
            lex_stream.push((span.start, t, span.end));
        }
        let parser = meerkat::ProgParser::new();
        let res = parser.parse(&input, &mut interner, lex_stream);
        assert!(matches!(
            res,
            Err(ParseError::User { ref error }) if error.contains("Assertion text exceeds maximum length")
        ));
    }

    /// Verify that parsing a table declaration with unified `int` type
    /// is successful
    #[test]
    fn test_parse_table_definition() {
        use crate::ast::{Decl, Stmt, TableType};

        let mut interner = Interner::new();
        let input = "service test_srv { \
            table test_tbl { \
                id: int, \
                name: string, \
                active: bool, \
            }; \
        }";
        let res = parse_string(input, &mut interner);
        assert!(res.is_ok());
        let ast = res.unwrap();
        assert_eq!(ast.len(), 1);
        if let Stmt::Service { decls, .. } = &ast[0] {
            assert_eq!(decls.len(), 1);
            if let Decl::TableDecl { fields, .. } = &decls[0] {
                assert_eq!(fields.len(), 3);
                assert_eq!(fields[0].ty, TableType::Int);
                assert_eq!(fields[1].ty, TableType::String);
                assert_eq!(fields[2].ty, TableType::Bool);
            } else {
                panic!("Expected TableDecl");
            }
        } else {
            panic!("Expected Service Stmt");
        }
    }

    /// #39: verify the html-part splitter produces literal text and parsed
    /// interpolation expressions in order.
    #[test]
    fn test_parse_html_parts_interpolation() {
        use crate::ast::{Expr, HtmlPart};
        let mut interner = Interner::new();
        let parts = parse_html_parts("The count is {count}.", &mut interner).expect("should parse");
        assert_eq!(parts.len(), 3, "expected text, expr, text: {:?}", parts);
        match &parts[0] {
            HtmlPart::Text(t) => assert_eq!(t, "The count is "),
            other => panic!("expected Text, got {:?}", other),
        }
        match &parts[1] {
            HtmlPart::Expr(e) => assert!(
                matches!(**e, Expr::Variable { .. }),
                "expected Variable, got {:?}",
                e
            ),
            other => panic!("expected Expr, got {:?}", other),
        }
        match &parts[2] {
            HtmlPart::Text(t) => assert_eq!(t, "."),
            other => panic!("expected Text, got {:?}", other),
        }
    }

    /// #39: no interpolation yields a single text part.
    #[test]
    fn test_parse_html_parts_text_only() {
        use crate::ast::HtmlPart;
        let mut interner = Interner::new();
        let parts = parse_html_parts("<p>hello</p>", &mut interner).expect("should parse");
        assert_eq!(parts.len(), 1);
        assert!(matches!(&parts[0], HtmlPart::Text(t) if t == "<p>hello</p>"));
    }

    /// #39: an unterminated interpolation is an error, not a panic.
    #[test]
    fn test_parse_html_parts_unterminated() {
        let mut interner = Interner::new();
        let res = parse_html_parts("count is {count", &mut interner);
        assert!(res.is_err());
    }

    /// #39: the html literal lexes and parses through to an Expr::Html node.
    /// (Stub stage: parts are empty; interpolation wiring is verified separately.)
    #[test]
    fn test_html_literal_parses() {
        use crate::ast::{Decl, Expr, Stmt};
        let mut interner = Interner::new();
        let input = "service s { pub def h = (<p>hi</p>); }";
        let res = parse_string(input, &mut interner);
        assert!(res.is_ok(), "parse failed: {:?}", res);
        let ast = res.unwrap();
        match &ast[0] {
            Stmt::Service { decls, .. } => match &decls[0] {
                Decl::DefDecl { val, .. } => {
                    assert!(
                        matches!(val, Expr::Html { .. }),
                        "expected Expr::Html, got {:?}",
                        val
                    );
                }
                other => panic!("expected DefDecl, got {:?}", other),
            },
            other => panic!("expected Service, got {:?}", other),
        }
    }

    /// #39: the full pipeline parses an html def with an interpolation into an
    /// Expr::Html whose parts include a parsed embedded expression.
    #[test]
    fn test_html_literal_full_parse() {
        use crate::ast::{Decl, Expr, HtmlPart, Stmt};
        let mut interner = Interner::new();
        let input = "service webClient { pub def html = (<p>The count is {count}.</p>); }";
        let res = parse_string(input, &mut interner);
        assert!(res.is_ok(), "parse failed: {:?}", res);
        let ast = res.unwrap();
        let parts = match &ast[0] {
            Stmt::Service { decls, .. } => match &decls[0] {
                Decl::DefDecl {
                    val: Expr::Html { parts },
                    ..
                } => parts,
                other => panic!("expected DefDecl with Expr::Html, got {:?}", other),
            },
            other => panic!("expected Service, got {:?}", other),
        };
        // <p>The count is , {count}, .</p>
        assert_eq!(parts.len(), 3, "parts: {:?}", parts);
        assert!(matches!(&parts[0], HtmlPart::Text(t) if t == "<p>The count is "));
        assert!(matches!(&parts[1], HtmlPart::Expr(e) if matches!(**e, Expr::Variable { .. })));
        assert!(matches!(&parts[2], HtmlPart::Text(t) if t == ".</p>"));
    }
}
