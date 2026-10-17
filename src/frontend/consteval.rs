//! Build-time evaluation of the expressions that decide hardware shape:
//! `uint<{ N + 1 }>` widths, instance parameters, and the bounds of a static
//! `for`.
//!
//! These expressions are evaluated by the frontend while compiling, so the
//! generic parameters disappear before Verilog is written (REQ-MVP-013). Only
//! const parameters and literals may appear - a signal value in a width is a
//! bug, and saying so is the whole point.

use std::collections::BTreeMap;

/// Largest supported value width.
///
/// The limit exists so that a typo such as `uint<100000000>` fails with a clear
/// message instead of exhausting memory while building a bit vector.
pub const MAX_WIDTH: u64 = 65536;

/// Environment of const generic parameters bound to concrete values.
pub(crate) type ConstEnv = BTreeMap<String, u64>;

/// Evaluate a build-time integer expression.
pub(crate) fn eval(expr: &syn::Expr, params: &ConstEnv) -> Result<u64, String> {
    match expr {
        syn::Expr::Lit(literal) => match &literal.lit {
            syn::Lit::Int(int) => int_value(int),
            syn::Lit::Bool(value) => Ok(u64::from(value.value)),
            other => Err(format!("{} is not an integer constant", describe_lit(other))),
        },
        syn::Expr::Path(path) => {
            if path.qself.is_some() || path.path.leading_colon.is_some() || path.path.segments.len() != 1 {
                return Err(
                    "a build-time expression may only name a const parameter, not a path with `::`".to_string(),
                );
            }
            let name = path.path.segments[0].ident.to_string();
            match params.get(&name) {
                Some(value) => Ok(*value),
                None => Err(format!(
                    "`{name}` is not a const parameter of this module; \
                     build-time expressions may only use const parameters and literals"
                )),
            }
        }
        syn::Expr::Binary(binary) => {
            let lhs = eval(&binary.left, params)?;
            let rhs = eval(&binary.right, params)?;
            let result = match binary.op {
                syn::BinOp::Add(_) => lhs.checked_add(rhs),
                syn::BinOp::Sub(_) => lhs.checked_sub(rhs),
                syn::BinOp::Mul(_) => lhs.checked_mul(rhs),
                syn::BinOp::Div(_) => {
                    if rhs == 0 {
                        return Err("division by zero in a build-time expression".to_string());
                    }
                    lhs.checked_div(rhs)
                }
                syn::BinOp::Rem(_) => {
                    if rhs == 0 {
                        return Err("remainder by zero in a build-time expression".to_string());
                    }
                    lhs.checked_rem(rhs)
                }
                syn::BinOp::BitAnd(_) => Some(lhs & rhs),
                syn::BinOp::BitOr(_) => Some(lhs | rhs),
                syn::BinOp::BitXor(_) => Some(lhs ^ rhs),
                syn::BinOp::Shl(_) => lhs.checked_shl(u32::try_from(rhs).unwrap_or(u32::MAX)),
                syn::BinOp::Shr(_) => lhs.checked_shr(u32::try_from(rhs).unwrap_or(u32::MAX)),
                _ => {
                    return Err(format!(
                        "operator `{}` is not supported in a build-time expression",
                        operator_symbol(&binary.op)
                    ))
                }
            };
            result.ok_or_else(|| {
                format!(
                    "build-time expression overflows ({} in a build-time expression)",
                    crate::frontend::parse::describe_expr(expr)
                )
            })
        }
        syn::Expr::Unary(unary) => {
            match unary.op {
                // `!` is logical on a `bool` and bitwise on an integer, exactly
                // as in Rust. The operand decides which one this is, so the two
                // are told apart instead of picking one.
                syn::UnOp::Not(_) => match bool_constant(&unary.expr)? {
                    Some(value) => Ok(u64::from(!value)),
                    None => Ok(!eval(&unary.expr, params)?),
                },
                syn::UnOp::Neg(_) => {
                    Err("negative build-time constants are not supported (widths are unsigned)".to_string())
                }
                _ => Err("unsupported unary operator in a build-time expression".to_string()),
            }
        }
        syn::Expr::Paren(paren) => eval(&paren.expr, params),
        syn::Expr::Group(group) => eval(&group.expr, params),
        syn::Expr::Block(block) => {
            // `{ N + 1 }`: the braced spelling Rust requires for a const generic
            // argument.
            let mut statements = block.block.stmts.iter();
            match (statements.next(), statements.next()) {
                (Some(syn::Stmt::Expr(value, _)), None) => eval(value, params),
                _ => Err("a braced build-time expression must contain exactly one expression".to_string()),
            }
        }
        syn::Expr::Cast(cast) => Err(format!(
            "`as` is not allowed in a build-time expression ({}): a cast would change the value silently; \
             write the arithmetic you mean",
            crate::frontend::parse::describe_type(&cast.ty)
        )),
        other => Err(format!(
            "{} is not a build-time constant expression (only const parameters, literals and arithmetic are)",
            crate::frontend::parse::describe_expr(other)
        )),
    }
}

/// The boolean value of a build-time expression, when it is a boolean one.
///
/// `None` means "an integer": `!` on it is bitwise, not logical.
fn bool_constant(expr: &syn::Expr) -> Result<Option<bool>, String> {
    match expr {
        syn::Expr::Lit(literal) => match &literal.lit {
            syn::Lit::Bool(value) => Ok(Some(value.value)),
            _ => Ok(None),
        },
        syn::Expr::Paren(paren) => bool_constant(&paren.expr),
        syn::Expr::Group(group) => bool_constant(&group.expr),
        syn::Expr::Block(block) => {
            let mut statements = block.block.stmts.iter();
            match (statements.next(), statements.next()) {
                (Some(syn::Stmt::Expr(value, _)), None) => bool_constant(value),
                _ => Ok(None),
            }
        }
        syn::Expr::Unary(unary) => match unary.op {
            syn::UnOp::Not(_) => Ok(bool_constant(&unary.expr)?.map(|value| !value)),
            _ => Ok(None),
        },
        syn::Expr::Binary(binary) => match binary.op {
            syn::BinOp::And(_) | syn::BinOp::Or(_) => {
                let lhs = bool_constant(&binary.left)?;
                let rhs = bool_constant(&binary.right)?;
                match (lhs, rhs) {
                    (Some(lhs), Some(rhs)) => {
                        Ok(Some(if matches!(binary.op, syn::BinOp::And(_)) {
                            lhs && rhs
                        } else {
                            lhs || rhs
                        }))
                    }
                    _ => Ok(None),
                }
            }
            _ => Ok(None),
        },
        _ => Ok(None),
    }
}

/// Evaluate and check a value width: at least one bit, at most [`MAX_WIDTH`].
pub(crate) fn eval_width(expr: &syn::Expr, params: &ConstEnv) -> Result<u32, String> {
    let value = eval(expr, params)?;
    check_width(value)
}

/// Check a width value produced by [`eval`].
pub(crate) fn check_width(value: u64) -> Result<u32, String> {
    if value == 0 {
        return Err("a width must be at least 1 bit".to_string());
    }
    if value > MAX_WIDTH {
        return Err(format!(
            "width {value} exceeds the maximum supported width of {MAX_WIDTH} bits"
        ));
    }
    Ok(value as u32)
}

/// Human-readable kind of a literal, for diagnostics.
fn describe_lit(lit: &syn::Lit) -> &'static str {
    match lit {
        syn::Lit::Str(_) => "a string literal",
        syn::Lit::ByteStr(_) => "a byte string literal",
        syn::Lit::CStr(_) => "a C string literal",
        syn::Lit::Byte(_) => "a byte literal",
        syn::Lit::Char(_) => "a character literal",
        syn::Lit::Float(_) => "a floating point literal",
        syn::Lit::Bool(_) => "a boolean literal",
        syn::Lit::Int(_) => "an integer literal",
        _ => "an unsupported literal",
    }
}

/// The value of an integer literal, in any radix and with any suffix.
pub(crate) fn int_value(literal: &syn::LitInt) -> Result<u64, String> {
    let text = literal.to_string();
    let suffix = literal.suffix();
    let digits = match text.strip_suffix(suffix) {
        Some(digits) => digits,
        None => text.as_str(),
    };
    let cleaned: String = digits.chars().filter(|c| *c != '_').collect();
    let (radix, body) = if let Some(rest) = cleaned
        .strip_prefix("0x")
        .or_else(|| cleaned.strip_prefix("0X"))
    {
        (16, rest)
    } else if let Some(rest) = cleaned
        .strip_prefix("0b")
        .or_else(|| cleaned.strip_prefix("0B"))
    {
        (2, rest)
    } else if let Some(rest) = cleaned
        .strip_prefix("0o")
        .or_else(|| cleaned.strip_prefix("0O"))
    {
        (8, rest)
    } else {
        (10, cleaned.as_str())
    };
    u64::from_str_radix(body, radix)
        .map_err(|_| format!("`{text}` is not a valid build-time integer (it must fit in 64 bits)"))
}

/// The symbol of a binary operator, for diagnostics.
fn operator_symbol(op: &syn::BinOp) -> &'static str {
    match op {
        syn::BinOp::Add(_) => "+",
        syn::BinOp::Sub(_) => "-",
        syn::BinOp::Mul(_) => "*",
        syn::BinOp::Div(_) => "/",
        syn::BinOp::Rem(_) => "%",
        syn::BinOp::And(_) => "&&",
        syn::BinOp::Or(_) => "||",
        syn::BinOp::BitXor(_) => "^",
        syn::BinOp::BitAnd(_) => "&",
        syn::BinOp::BitOr(_) => "|",
        syn::BinOp::Shl(_) => "<<",
        syn::BinOp::Shr(_) => ">>",
        syn::BinOp::Eq(_) => "==",
        syn::BinOp::Lt(_) => "<",
        syn::BinOp::Le(_) => "<=",
        syn::BinOp::Ne(_) => "!=",
        syn::BinOp::Ge(_) => ">=",
        syn::BinOp::Gt(_) => ">",
        _ => "that compound operator",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expr(source: &str) -> syn::Expr {
        syn::parse_str(source).expect("test expression parses")
    }

    fn env() -> ConstEnv {
        let mut env = ConstEnv::new();
        env.insert("N".to_string(), 8);
        env
    }

    #[test]
    fn arithmetic_over_parameters_is_evaluated() {
        assert_eq!(eval(&expr("{ N + 1 }"), &env()).unwrap(), 9);
        assert_eq!(eval(&expr("N * 2"), &env()).unwrap(), 16);
        assert_eq!(eval(&expr("(N + 2) / 2"), &env()).unwrap(), 5);
    }

    #[test]
    fn literals_in_every_radix_are_understood() {
        assert_eq!(eval(&expr("8"), &env()).unwrap(), 8);
        assert_eq!(eval(&expr("0xFFusize"), &env()).unwrap(), 255);
        assert_eq!(eval(&expr("0b1010"), &env()).unwrap(), 10);
        assert_eq!(eval(&expr("1_000"), &env()).unwrap(), 1000);
    }

    #[test]
    fn signals_and_unknown_names_are_rejected() {
        assert!(eval(&expr("self.a"), &env()).is_err());
        let error = eval(&expr("M"), &env()).unwrap_err();
        assert!(error.contains("not a const parameter"), "{error}");
    }

    #[test]
    fn division_by_zero_and_overflow_are_reported() {
        assert!(eval(&expr("N / 0"), &env())
            .unwrap_err()
            .contains("division by zero"));
        let error = eval(&expr("18446744073709551615 + 1"), &env()).unwrap_err();
        assert!(error.contains("overflows"), "{error}");
        assert!(eval(&expr("0xFFFFFFFFFFFFFFFF + 1"), &env()).is_err());
    }

    #[test]
    fn widths_are_checked_not_truncated() {
        assert!(check_width(8).is_ok());
        assert!(check_width(0).unwrap_err().contains("at least 1"));
        assert!(check_width(MAX_WIDTH + 1)
            .unwrap_err()
            .contains("maximum supported"));
    }
}
