use std::cmp::Ordering;

/// A dynamically-typed value read from Markdown YAML frontmatter.
///
/// This is the common currency for query evaluation: frontmatter fields are
/// parsed into `Value`s, and the query engine (comparisons, `ORDER BY`,
/// `MIN`/`MAX`) operates on them without knowing the original YAML shape.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<Value>),
}

impl Value {
    /// True for `Value::Null`.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Human-readable rendering used by the REPL's table/CSV output.
    ///
    /// `Null` renders as an empty string; `List` joins each element's own
    /// `display()` with `", "`.
    pub fn display(&self) -> String {
        match self {
            Value::Null => String::new(),
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => f.to_string(),
            Value::Str(s) => s.clone(),
            Value::List(items) => items
                .iter()
                .map(Value::display)
                .collect::<Vec<_>>()
                .join(", "),
        }
    }

    /// Coerces this value to `f64` for numeric comparisons, if possible.
    ///
    /// `Int`/`Float` convert directly; `Str` is parsed (after trimming
    /// whitespace) as a float. Everything else — including `Bool`, `Null`,
    /// and `List` — yields `None`.
    pub fn as_number(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            Value::Str(s) => s.trim().parse::<f64>().ok(),
            Value::Bool(_) | Value::Null | Value::List(_) => None,
        }
    }

    /// Canonical string form used for lexicographic comparison.
    pub fn to_cmp_string(&self) -> String {
        self.display()
    }
}

/// Total-ish ordering used by `ORDER BY`/`MIN`/`MAX`.
///
/// Values that both coerce to a number compare numerically; otherwise they
/// compare lexicographically on `to_cmp_string()`. Comparing `Null` against
/// anything (including another `Null`) returns `None` — callers are
/// responsible for placing NULLs last.
pub fn compare_values(a: &Value, b: &Value) -> Option<Ordering> {
    if a.is_null() || b.is_null() {
        return None;
    }
    match (a.as_number(), b.as_number()) {
        (Some(x), Some(y)) => x.partial_cmp(&y),
        _ => Some(a.to_cmp_string().cmp(&b.to_cmp_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    #[test]
    fn display_null_is_empty() {
        assert_eq!(Value::Null.display(), "");
    }
    #[test]
    fn display_list_is_comma_joined() {
        let v = Value::List(vec![Value::Str("a".into()), Value::Int(2)]);
        assert_eq!(v.display(), "a, 2");
    }
    #[test]
    fn display_scalars() {
        assert_eq!(Value::Str("x".into()).display(), "x");
        assert_eq!(Value::Int(10).display(), "10");
        assert_eq!(Value::Bool(true).display(), "true");
        assert_eq!(Value::Float(1.5).display(), "1.5");
    }
    #[test]
    fn as_number_coerces_numeric_strings() {
        assert_eq!(Value::Int(3).as_number(), Some(3.0));
        assert_eq!(Value::Str("3".into()).as_number(), Some(3.0));
        assert_eq!(Value::Str("x".into()).as_number(), None);
        assert_eq!(Value::Null.as_number(), None);
    }
    #[test]
    fn compare_numbers_numerically() {
        assert_eq!(
            compare_values(&Value::Int(2), &Value::Int(10)),
            Some(Ordering::Less)
        );
        // numeric string vs int compares numerically
        assert_eq!(
            compare_values(&Value::Str("2".into()), &Value::Int(10)),
            Some(Ordering::Less)
        );
    }
    #[test]
    fn compare_strings_lexicographically() {
        assert_eq!(
            compare_values(&Value::Str("a".into()), &Value::Str("b".into())),
            Some(Ordering::Less)
        );
    }
    #[test]
    fn compare_null_is_none() {
        assert_eq!(compare_values(&Value::Null, &Value::Int(1)), None);
        assert_eq!(compare_values(&Value::Int(1), &Value::Null), None);
    }
}
