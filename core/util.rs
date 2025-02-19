use limbo_sqlite3_parser::ast::{self, CreateTableBody, Expr, FunctionTail, Literal};
use std::{rc::Rc, sync::Arc};

use crate::{
    schema::{self, Column, Schema, Type},
    LimboError, OpenFlags, Result, Statement, StepResult, IO,
};

// https://sqlite.org/lang_keywords.html
const QUOTE_PAIRS: &[(char, char)] = &[('"', '"'), ('[', ']'), ('`', '`')];

pub fn normalize_ident(identifier: &str) -> String {
    let quote_pair = QUOTE_PAIRS
        .iter()
        .find(|&(start, end)| identifier.starts_with(*start) && identifier.ends_with(*end));

    if let Some(&(_, _)) = quote_pair {
        &identifier[1..identifier.len() - 1]
    } else {
        identifier
    }
    .to_lowercase()
}

pub const PRIMARY_KEY_AUTOMATIC_INDEX_NAME_PREFIX: &str = "sqlite_autoindex_";

pub fn parse_schema_rows(
    rows: Option<Statement>,
    schema: &mut Schema,
    io: Arc<dyn IO>,
) -> Result<()> {
    if let Some(mut rows) = rows {
        let mut automatic_indexes = Vec::new();
        loop {
            match rows.step()? {
                StepResult::Row => {
                    let row = rows.row().unwrap();
                    let ty = row.get::<&str>(0)?;
                    if ty != "table" && ty != "index" {
                        continue;
                    }
                    match ty {
                        "table" => {
                            let root_page: i64 = row.get::<i64>(3)?;
                            let sql: &str = row.get::<&str>(4)?;
                            let table = schema::BTreeTable::from_sql(sql, root_page as usize)?;
                            schema.add_table(Rc::new(table));
                        }
                        "index" => {
                            let root_page: i64 = row.get::<i64>(3)?;
                            match row.get::<&str>(4) {
                                Ok(sql) => {
                                    let index = schema::Index::from_sql(sql, root_page as usize)?;
                                    schema.add_index(Rc::new(index));
                                }
                                _ => {
                                    // Automatic index on primary key, e.g.
                                    // table|foo|foo|2|CREATE TABLE foo (a text PRIMARY KEY, b)
                                    // index|sqlite_autoindex_foo_1|foo|3|
                                    let index_name = row.get::<&str>(1)?;
                                    let table_name = row.get::<&str>(2)?;
                                    let root_page = row.get::<i64>(3)?;
                                    automatic_indexes.push((
                                        index_name.to_string(),
                                        table_name.to_string(),
                                        root_page,
                                    ));
                                }
                            }
                        }
                        _ => continue,
                    }
                }
                StepResult::IO => {
                    // TODO: How do we ensure that the I/O we submitted to
                    // read the schema is actually complete?
                    io.run_once()?;
                }
                StepResult::Interrupt => break,
                StepResult::Done => break,
                StepResult::Busy => break,
            }
        }
        for (index_name, table_name, root_page) in automatic_indexes {
            // We need to process these after all tables are loaded into memory due to the schema.get_table() call
            let table = schema.get_table(&table_name).unwrap();
            let index =
                schema::Index::automatic_from_primary_key(&table, &index_name, root_page as usize)?;
            schema.add_index(Rc::new(index));
        }
    }
    Ok(())
}

fn cmp_numeric_strings(num_str: &str, other: &str) -> bool {
    match (num_str.parse::<f64>(), other.parse::<f64>()) {
        (Ok(num), Ok(other)) => num == other,
        _ => num_str == other,
    }
}

pub fn check_ident_equivalency(ident1: &str, ident2: &str) -> bool {
    fn strip_quotes(identifier: &str) -> &str {
        for &(start, end) in QUOTE_PAIRS {
            if identifier.starts_with(start) && identifier.ends_with(end) {
                return &identifier[1..identifier.len() - 1];
            }
        }
        identifier
    }
    strip_quotes(ident1).eq_ignore_ascii_case(strip_quotes(ident2))
}

pub fn check_literal_equivalency(lhs: &Literal, rhs: &Literal) -> bool {
    match (lhs, rhs) {
        (Literal::Numeric(n1), Literal::Numeric(n2)) => cmp_numeric_strings(n1, n2),
        (Literal::String(s1), Literal::String(s2)) => check_ident_equivalency(s1, s2),
        (Literal::Blob(b1), Literal::Blob(b2)) => b1 == b2,
        (Literal::Keyword(k1), Literal::Keyword(k2)) => check_ident_equivalency(k1, k2),
        (Literal::Null, Literal::Null) => true,
        (Literal::CurrentDate, Literal::CurrentDate) => true,
        (Literal::CurrentTime, Literal::CurrentTime) => true,
        (Literal::CurrentTimestamp, Literal::CurrentTimestamp) => true,
        _ => false,
    }
}

/// This function is used to determine whether two expressions are logically
/// equivalent in the context of queries, even if their representations
/// differ. e.g.: `SUM(x)` and `sum(x)`, `x + y` and `y + x`
///
/// *Note*: doesn't attempt to evaluate/compute "constexpr" results
pub fn exprs_are_equivalent(expr1: &Expr, expr2: &Expr) -> bool {
    match (expr1, expr2) {
        (
            Expr::Between {
                lhs: lhs1,
                not: not1,
                start: start1,
                end: end1,
            },
            Expr::Between {
                lhs: lhs2,
                not: not2,
                start: start2,
                end: end2,
            },
        ) => {
            not1 == not2
                && exprs_are_equivalent(lhs1, lhs2)
                && exprs_are_equivalent(start1, start2)
                && exprs_are_equivalent(end1, end2)
        }
        (Expr::Binary(lhs1, op1, rhs1), Expr::Binary(lhs2, op2, rhs2)) => {
            op1 == op2
                && ((exprs_are_equivalent(lhs1, lhs2) && exprs_are_equivalent(rhs1, rhs2))
                    || (op1.is_commutative()
                        && exprs_are_equivalent(lhs1, rhs2)
                        && exprs_are_equivalent(rhs1, lhs2)))
        }
        (
            Expr::Case {
                base: base1,
                when_then_pairs: pairs1,
                else_expr: else1,
            },
            Expr::Case {
                base: base2,
                when_then_pairs: pairs2,
                else_expr: else2,
            },
        ) => {
            base1 == base2
                && pairs1.len() == pairs2.len()
                && pairs1.iter().zip(pairs2).all(|((w1, t1), (w2, t2))| {
                    exprs_are_equivalent(w1, w2) && exprs_are_equivalent(t1, t2)
                })
                && else1 == else2
        }
        (
            Expr::Cast {
                expr: expr1,
                type_name: type1,
            },
            Expr::Cast {
                expr: expr2,
                type_name: type2,
            },
        ) => {
            exprs_are_equivalent(expr1, expr2)
                && match (type1, type2) {
                    (Some(t1), Some(t2)) => t1.name.eq_ignore_ascii_case(&t2.name),
                    _ => false,
                }
        }
        (Expr::Collate(expr1, collation1), Expr::Collate(expr2, collation2)) => {
            exprs_are_equivalent(expr1, expr2) && collation1.eq_ignore_ascii_case(collation2)
        }
        (
            Expr::FunctionCall {
                name: name1,
                distinctness: distinct1,
                args: args1,
                order_by: order1,
                filter_over: filter1,
            },
            Expr::FunctionCall {
                name: name2,
                distinctness: distinct2,
                args: args2,
                order_by: order2,
                filter_over: filter2,
            },
        ) => {
            name1.0.eq_ignore_ascii_case(&name2.0)
                && distinct1 == distinct2
                && args1 == args2
                && order1 == order2
                && filter1 == filter2
        }
        (
            Expr::FunctionCallStar {
                name: name1,
                filter_over: filter1,
            },
            Expr::FunctionCallStar {
                name: name2,
                filter_over: filter2,
            },
        ) => {
            name1.0.eq_ignore_ascii_case(&name2.0)
                && match (filter1, filter2) {
                    (None, None) => true,
                    (
                        Some(FunctionTail {
                            filter_clause: fc1,
                            over_clause: oc1,
                        }),
                        Some(FunctionTail {
                            filter_clause: fc2,
                            over_clause: oc2,
                        }),
                    ) => match ((fc1, fc2), (oc1, oc2)) {
                        ((Some(fc1), Some(fc2)), (Some(oc1), Some(oc2))) => {
                            exprs_are_equivalent(fc1, fc2) && oc1 == oc2
                        }
                        ((Some(fc1), Some(fc2)), _) => exprs_are_equivalent(fc1, fc2),
                        _ => false,
                    },
                    _ => false,
                }
        }
        (Expr::NotNull(expr1), Expr::NotNull(expr2)) => exprs_are_equivalent(expr1, expr2),
        (Expr::IsNull(expr1), Expr::IsNull(expr2)) => exprs_are_equivalent(expr1, expr2),
        (Expr::Literal(lit1), Expr::Literal(lit2)) => check_literal_equivalency(lit1, lit2),
        (Expr::Id(id1), Expr::Id(id2)) => check_ident_equivalency(&id1.0, &id2.0),
        (Expr::Unary(op1, expr1), Expr::Unary(op2, expr2)) => {
            op1 == op2 && exprs_are_equivalent(expr1, expr2)
        }
        (Expr::Variable(var1), Expr::Variable(var2)) => var1 == var2,
        (Expr::Parenthesized(exprs1), Expr::Parenthesized(exprs2)) => {
            exprs1.len() == exprs2.len()
                && exprs1
                    .iter()
                    .zip(exprs2)
                    .all(|(e1, e2)| exprs_are_equivalent(e1, e2))
        }
        (Expr::Parenthesized(exprs1), exprs2) | (exprs2, Expr::Parenthesized(exprs1)) => {
            exprs1.len() == 1 && exprs_are_equivalent(&exprs1[0], exprs2)
        }
        (Expr::Qualified(tn1, cn1), Expr::Qualified(tn2, cn2)) => {
            check_ident_equivalency(&tn1.0, &tn2.0) && check_ident_equivalency(&cn1.0, &cn2.0)
        }
        (Expr::DoublyQualified(sn1, tn1, cn1), Expr::DoublyQualified(sn2, tn2, cn2)) => {
            check_ident_equivalency(&sn1.0, &sn2.0)
                && check_ident_equivalency(&tn1.0, &tn2.0)
                && check_ident_equivalency(&cn1.0, &cn2.0)
        }
        (
            Expr::InList {
                lhs: lhs1,
                not: not1,
                rhs: rhs1,
            },
            Expr::InList {
                lhs: lhs2,
                not: not2,
                rhs: rhs2,
            },
        ) => {
            *not1 == *not2
                && exprs_are_equivalent(lhs1, lhs2)
                && rhs1
                    .as_ref()
                    .zip(rhs2.as_ref())
                    .map(|(list1, list2)| {
                        list1.len() == list2.len()
                            && list1
                                .iter()
                                .zip(list2)
                                .all(|(e1, e2)| exprs_are_equivalent(e1, e2))
                    })
                    .unwrap_or(false)
        }
        // fall back to naive equality check
        _ => expr1 == expr2,
    }
}

pub fn columns_from_create_table_body(body: ast::CreateTableBody) -> Result<Vec<Column>, ()> {
    let CreateTableBody::ColumnsAndConstraints { columns, .. } = body else {
        return Err(());
    };

    Ok(columns
        .into_iter()
        .filter_map(|(name, column_def)| {
            // if column_def.col_type includes HIDDEN, omit it for now
            if let Some(data_type) = column_def.col_type.as_ref() {
                if data_type.name.as_str().contains("HIDDEN") {
                    return None;
                }
            }
            let column = Column {
                name: Some(name.0),
                ty: match column_def.col_type {
                    Some(ref data_type) => {
                        // https://www.sqlite.org/datatype3.html
                        let type_name = data_type.name.as_str().to_uppercase();
                        if type_name.contains("INT") {
                            Type::Integer
                        } else if type_name.contains("CHAR")
                            || type_name.contains("CLOB")
                            || type_name.contains("TEXT")
                        {
                            Type::Text
                        } else if type_name.contains("BLOB") || type_name.is_empty() {
                            Type::Blob
                        } else if type_name.contains("REAL")
                            || type_name.contains("FLOA")
                            || type_name.contains("DOUB")
                        {
                            Type::Real
                        } else {
                            Type::Numeric
                        }
                    }
                    None => Type::Null,
                },
                default: column_def
                    .constraints
                    .iter()
                    .find_map(|c| match &c.constraint {
                        limbo_sqlite3_parser::ast::ColumnConstraint::Default(val) => {
                            Some(val.clone())
                        }
                        _ => None,
                    }),
                notnull: column_def.constraints.iter().any(|c| {
                    matches!(
                        c.constraint,
                        limbo_sqlite3_parser::ast::ColumnConstraint::NotNull { .. }
                    )
                }),
                ty_str: column_def
                    .col_type
                    .clone()
                    .map(|t| t.name.to_string())
                    .unwrap_or_default(),
                primary_key: column_def.constraints.iter().any(|c| {
                    matches!(
                        c.constraint,
                        limbo_sqlite3_parser::ast::ColumnConstraint::PrimaryKey { .. }
                    )
                }),
                is_rowid_alias: false,
            };
            Some(column)
        })
        .collect::<Vec<_>>())
}

#[derive(Debug, Default, PartialEq)]
pub struct OpenOptions<'a> {
    pub authority: Option<&'a str>,
    pub path: String,
    pub fragment: Option<String>,
    pub vfs: Option<String>,
    pub mode: Mode,
    pub modeof: Option<String>,
    pub cache: Option<CacheMode>,
    pub immutable: Option<bool>,
}

#[derive(Clone, Default, Debug, Copy, PartialEq)]
pub enum Mode {
    ReadOnly,
    ReadWrite,
    Memory,
    #[default]
    ReadWriteCreate,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CacheMode {
    Private,
    Shared,
}

impl From<&str> for CacheMode {
    fn from(s: &str) -> Self {
        match s {
            "private" => CacheMode::Private,
            "shared" => CacheMode::Shared,
            _ => CacheMode::Private,
        }
    }
}

impl Mode {
    pub fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_lowercase().as_str() {
            "ro" => Ok(Mode::ReadOnly),
            "rw" => Ok(Mode::ReadWrite),
            "memory" => Ok(Mode::Memory),
            "rwc" => Ok(Mode::ReadWriteCreate),
            _ => Err(LimboError::InvalidArgument(format!(
                "Invalid mode: '{}'. Expected one of 'ro', 'rw', 'memory', 'rwc'",
                s
            ))),
        }
    }
    pub fn get_flags(&self) -> OpenFlags {
        match self {
            Mode::ReadWriteCreate => OpenFlags::Create,
            _ => OpenFlags::None,
        }
    }
}

fn is_windows_path(path: &str) -> bool {
    path.len() >= 3 && path.chars().nth(1) == Some(':') && path.chars().nth(2) == Some('/')
}

/// Parses a SQLite URI, handling Windows and Unix paths separately.
pub fn parse_sqlite_uri(uri: &str) -> Result<OpenOptions> {
    if !uri.starts_with("file:") {
        return Ok(OpenOptions {
            path: uri.to_string(),
            ..Default::default()
        });
    }

    let mut opts = OpenOptions::default();
    let without_scheme = &uri[5..];

    let (without_fragment, fragment) = without_scheme
        .split_once('#')
        .unwrap_or((without_scheme, ""));
    if !fragment.is_empty() {
        opts.fragment = Some(decode_percent(fragment));
    }

    let (without_query, query) = without_fragment
        .split_once('?')
        .unwrap_or((without_fragment, ""));
    parse_query_params(query, &mut opts)?;

    // Handle authority + path separately
    if let Some(after_slashes) = without_query.strip_prefix("//") {
        let (authority, path) = after_slashes.split_once('/').unwrap_or((after_slashes, ""));

        // SQLite allows only `localhost` or empty authority.
        if !(authority.is_empty() || authority == "localhost") {
            return Err(LimboError::InvalidArgument(format!(
                "Invalid authority '{}'. Only '' or 'localhost' allowed.",
                authority
            )));
        }
        opts.authority = if authority.is_empty() {
            None
        } else {
            Some(authority)
        };

        if is_windows_path(path) {
            opts.path = format!("/{}", decode_percent(path)); // Ensure `/C:/` format
        } else if !path.is_empty() {
            opts.path = format!("/{}", decode_percent(path));
        } else {
            opts.path = String::new();
        }
    } else {
        // no authority, must be a normal absolute or relative path.
        opts.path = decode_percent(without_query);
    }

    Ok(opts)
}

// parses query parameters and updates OpenOptions
fn parse_query_params(query: &str, opts: &mut OpenOptions) -> Result<()> {
    for param in query.split('&') {
        if let Some((key, value)) = param.split_once('=') {
            let decoded_value = decode_percent(value);
            match key {
                "mode" => opts.mode = Mode::from_str(value)?,
                "modeof" => opts.modeof = Some(decoded_value),
                "cache" => opts.cache = Some(decoded_value.as_str().into()),
                "immutable" => opts.immutable = Some(decoded_value == "1"),
                "vfs" => opts.vfs = Some(decoded_value),
                _ => {}
            }
        }
    }
    Ok(())
}

/// Decodes percent-encoded characters (e.g., `%20` → `' '`).
fn decode_percent(input: &str) -> String {
    let mut result = String::new();
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '%' {
            if let (Some(h1), Some(h2)) = (chars.next(), chars.next()) {
                if let Ok(byte) = u8::from_str_radix(&format!("{}{}", h1, h2), 16) {
                    result.push(byte as char);
                } else {
                    result.push('%');
                    result.push(h1);
                    result.push(h2);
                }
            } else {
                result.push('%');
            }
        } else {
            result.push(c);
        }
    }
    result
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use limbo_sqlite3_parser::ast::{self, Expr, Id, Literal, Operator::*, Type};

    #[test]
    fn test_normalize_ident() {
        assert_eq!(normalize_ident("foo"), "foo");
        assert_eq!(normalize_ident("`foo`"), "foo");
        assert_eq!(normalize_ident("[foo]"), "foo");
        assert_eq!(normalize_ident("\"foo\""), "foo");
    }

    #[test]
    fn test_basic_addition_exprs_are_equivalent() {
        let expr1 = Expr::Binary(
            Box::new(Expr::Literal(Literal::Numeric("826".to_string()))),
            Add,
            Box::new(Expr::Literal(Literal::Numeric("389".to_string()))),
        );
        let expr2 = Expr::Binary(
            Box::new(Expr::Literal(Literal::Numeric("389".to_string()))),
            Add,
            Box::new(Expr::Literal(Literal::Numeric("826".to_string()))),
        );
        assert!(exprs_are_equivalent(&expr1, &expr2));
    }

    #[test]
    fn test_addition_expressions_equivalent_normalized() {
        let expr1 = Expr::Binary(
            Box::new(Expr::Literal(Literal::Numeric("123.0".to_string()))),
            Add,
            Box::new(Expr::Literal(Literal::Numeric("243".to_string()))),
        );
        let expr2 = Expr::Binary(
            Box::new(Expr::Literal(Literal::Numeric("243.0".to_string()))),
            Add,
            Box::new(Expr::Literal(Literal::Numeric("123".to_string()))),
        );
        assert!(exprs_are_equivalent(&expr1, &expr2));
    }

    #[test]
    fn test_subtraction_expressions_not_equivalent() {
        let expr3 = Expr::Binary(
            Box::new(Expr::Literal(Literal::Numeric("364".to_string()))),
            Subtract,
            Box::new(Expr::Literal(Literal::Numeric("22.0".to_string()))),
        );
        let expr4 = Expr::Binary(
            Box::new(Expr::Literal(Literal::Numeric("22.0".to_string()))),
            Subtract,
            Box::new(Expr::Literal(Literal::Numeric("364".to_string()))),
        );
        assert!(!exprs_are_equivalent(&expr3, &expr4));
    }

    #[test]
    fn test_subtraction_expressions_normalized() {
        let expr3 = Expr::Binary(
            Box::new(Expr::Literal(Literal::Numeric("66.0".to_string()))),
            Subtract,
            Box::new(Expr::Literal(Literal::Numeric("22".to_string()))),
        );
        let expr4 = Expr::Binary(
            Box::new(Expr::Literal(Literal::Numeric("66".to_string()))),
            Subtract,
            Box::new(Expr::Literal(Literal::Numeric("22.0".to_string()))),
        );
        assert!(exprs_are_equivalent(&expr3, &expr4));
    }

    #[test]
    fn test_expressions_equivalent_case_insensitive_functioncalls() {
        let func1 = Expr::FunctionCall {
            name: Id("SUM".to_string()),
            distinctness: None,
            args: Some(vec![Expr::Id(Id("x".to_string()))]),
            order_by: None,
            filter_over: None,
        };
        let func2 = Expr::FunctionCall {
            name: Id("sum".to_string()),
            distinctness: None,
            args: Some(vec![Expr::Id(Id("x".to_string()))]),
            order_by: None,
            filter_over: None,
        };
        assert!(exprs_are_equivalent(&func1, &func2));

        let func3 = Expr::FunctionCall {
            name: Id("SUM".to_string()),
            distinctness: Some(ast::Distinctness::Distinct),
            args: Some(vec![Expr::Id(Id("x".to_string()))]),
            order_by: None,
            filter_over: None,
        };
        assert!(!exprs_are_equivalent(&func1, &func3));
    }

    #[test]
    fn test_expressions_equivalent_identical_fn_with_distinct() {
        let sum = Expr::FunctionCall {
            name: Id("SUM".to_string()),
            distinctness: None,
            args: Some(vec![Expr::Id(Id("x".to_string()))]),
            order_by: None,
            filter_over: None,
        };
        let sum_distinct = Expr::FunctionCall {
            name: Id("SUM".to_string()),
            distinctness: Some(ast::Distinctness::Distinct),
            args: Some(vec![Expr::Id(Id("x".to_string()))]),
            order_by: None,
            filter_over: None,
        };
        assert!(!exprs_are_equivalent(&sum, &sum_distinct));
    }

    #[test]
    fn test_expressions_equivalent_multiplication() {
        let expr1 = Expr::Binary(
            Box::new(Expr::Literal(Literal::Numeric("42.0".to_string()))),
            Multiply,
            Box::new(Expr::Literal(Literal::Numeric("38".to_string()))),
        );
        let expr2 = Expr::Binary(
            Box::new(Expr::Literal(Literal::Numeric("38.0".to_string()))),
            Multiply,
            Box::new(Expr::Literal(Literal::Numeric("42".to_string()))),
        );
        assert!(exprs_are_equivalent(&expr1, &expr2));
    }

    #[test]
    fn test_expressions_both_parenthesized_equivalent() {
        let expr1 = Expr::Parenthesized(vec![Expr::Binary(
            Box::new(Expr::Literal(Literal::Numeric("683".to_string()))),
            Add,
            Box::new(Expr::Literal(Literal::Numeric("799.0".to_string()))),
        )]);
        let expr2 = Expr::Binary(
            Box::new(Expr::Literal(Literal::Numeric("799".to_string()))),
            Add,
            Box::new(Expr::Literal(Literal::Numeric("683".to_string()))),
        );
        assert!(exprs_are_equivalent(&expr1, &expr2));
    }
    #[test]
    fn test_expressions_parenthesized_equivalent() {
        let expr7 = Expr::Parenthesized(vec![Expr::Binary(
            Box::new(Expr::Literal(Literal::Numeric("6".to_string()))),
            Add,
            Box::new(Expr::Literal(Literal::Numeric("7".to_string()))),
        )]);
        let expr8 = Expr::Binary(
            Box::new(Expr::Literal(Literal::Numeric("6".to_string()))),
            Add,
            Box::new(Expr::Literal(Literal::Numeric("7".to_string()))),
        );
        assert!(exprs_are_equivalent(&expr7, &expr8));
    }

    #[test]
    fn test_like_expressions_equivalent() {
        let expr1 = Expr::Like {
            lhs: Box::new(Expr::Id(Id("name".to_string()))),
            not: false,
            op: ast::LikeOperator::Like,
            rhs: Box::new(Expr::Literal(Literal::String("%john%".to_string()))),
            escape: Some(Box::new(Expr::Literal(Literal::String("\\".to_string())))),
        };
        let expr2 = Expr::Like {
            lhs: Box::new(Expr::Id(Id("name".to_string()))),
            not: false,
            op: ast::LikeOperator::Like,
            rhs: Box::new(Expr::Literal(Literal::String("%john%".to_string()))),
            escape: Some(Box::new(Expr::Literal(Literal::String("\\".to_string())))),
        };
        assert!(exprs_are_equivalent(&expr1, &expr2));
    }

    #[test]
    fn test_expressions_equivalent_like_escaped() {
        let expr1 = Expr::Like {
            lhs: Box::new(Expr::Id(Id("name".to_string()))),
            not: false,
            op: ast::LikeOperator::Like,
            rhs: Box::new(Expr::Literal(Literal::String("%john%".to_string()))),
            escape: Some(Box::new(Expr::Literal(Literal::String("\\".to_string())))),
        };
        let expr2 = Expr::Like {
            lhs: Box::new(Expr::Id(Id("name".to_string()))),
            not: false,
            op: ast::LikeOperator::Like,
            rhs: Box::new(Expr::Literal(Literal::String("%john%".to_string()))),
            escape: Some(Box::new(Expr::Literal(Literal::String("#".to_string())))),
        };
        assert!(!exprs_are_equivalent(&expr1, &expr2));
    }
    #[test]
    fn test_expressions_equivalent_between() {
        let expr1 = Expr::Between {
            lhs: Box::new(Expr::Id(Id("age".to_string()))),
            not: false,
            start: Box::new(Expr::Literal(Literal::Numeric("18".to_string()))),
            end: Box::new(Expr::Literal(Literal::Numeric("65".to_string()))),
        };
        let expr2 = Expr::Between {
            lhs: Box::new(Expr::Id(Id("age".to_string()))),
            not: false,
            start: Box::new(Expr::Literal(Literal::Numeric("18".to_string()))),
            end: Box::new(Expr::Literal(Literal::Numeric("65".to_string()))),
        };
        assert!(exprs_are_equivalent(&expr1, &expr2));

        // differing BETWEEN bounds
        let expr3 = Expr::Between {
            lhs: Box::new(Expr::Id(Id("age".to_string()))),
            not: false,
            start: Box::new(Expr::Literal(Literal::Numeric("20".to_string()))),
            end: Box::new(Expr::Literal(Literal::Numeric("65".to_string()))),
        };
        assert!(!exprs_are_equivalent(&expr1, &expr3));
    }
    #[test]
    fn test_cast_exprs_equivalent() {
        let cast1 = Expr::Cast {
            expr: Box::new(Expr::Literal(Literal::Numeric("123".to_string()))),
            type_name: Some(Type {
                name: "INTEGER".to_string(),
                size: None,
            }),
        };

        let cast2 = Expr::Cast {
            expr: Box::new(Expr::Literal(Literal::Numeric("123".to_string()))),
            type_name: Some(Type {
                name: "integer".to_string(),
                size: None,
            }),
        };
        assert!(exprs_are_equivalent(&cast1, &cast2));
    }

    #[test]
    fn test_ident_equivalency() {
        assert!(check_ident_equivalency("\"foo\"", "foo"));
        assert!(check_ident_equivalency("[foo]", "foo"));
        assert!(check_ident_equivalency("`FOO`", "foo"));
        assert!(check_ident_equivalency("\"foo\"", "`FOO`"));
        assert!(!check_ident_equivalency("\"foo\"", "[bar]"));
        assert!(!check_ident_equivalency("foo", "\"bar\""));
    }

    #[test]
    fn test_simple_uri() {
        let uri = "file:/home/user/db.sqlite";
        let opts = parse_sqlite_uri(uri).unwrap();
        assert_eq!(opts.path, "/home/user/db.sqlite");
        assert_eq!(opts.authority, None);
    }

    #[test]
    fn test_uri_with_authority() {
        let uri = "file://localhost/home/user/db.sqlite";
        let opts = parse_sqlite_uri(uri).unwrap();
        assert_eq!(opts.path, "/home/user/db.sqlite");
        assert_eq!(opts.authority, Some("localhost"));
    }

    #[test]
    fn test_uri_with_invalid_authority() {
        let uri = "file://example.com/home/user/db.sqlite";
        let result = parse_sqlite_uri(uri);
        assert!(result.is_err());
    }

    #[test]
    fn test_uri_with_query_params() {
        let uri = "file:/home/user/db.sqlite?vfs=unix&mode=ro&immutable=1";
        let opts = parse_sqlite_uri(uri).unwrap();
        assert_eq!(opts.path, "/home/user/db.sqlite");
        assert_eq!(opts.vfs, Some("unix".to_string()));
        assert_eq!(opts.mode, Mode::ReadOnly);
        assert_eq!(opts.immutable, Some(true));
    }

    #[test]
    fn test_uri_with_fragment() {
        let uri = "file:/home/user/db.sqlite#section1";
        let opts = parse_sqlite_uri(uri).unwrap();
        assert_eq!(opts.path, "/home/user/db.sqlite");
        assert_eq!(opts.fragment, Some("section1".to_string()));
    }

    #[test]
    fn test_uri_with_percent_encoding() {
        let uri = "file:/home/user/db%20with%20spaces.sqlite?vfs=unix";
        let opts = parse_sqlite_uri(uri).unwrap();
        assert_eq!(opts.path, "/home/user/db with spaces.sqlite");
        assert_eq!(opts.vfs, Some("unix".to_string()));
    }

    #[test]
    fn test_uri_without_scheme() {
        let uri = "/home/user/db.sqlite";
        let result = parse_sqlite_uri(uri);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().path, "/home/user/db.sqlite");
    }

    #[test]
    fn test_uri_with_empty_query() {
        let uri = "file:/home/user/db.sqlite?";
        let opts = parse_sqlite_uri(uri).unwrap();
        assert_eq!(opts.path, "/home/user/db.sqlite");
        assert_eq!(opts.vfs, None);
    }

    #[test]
    fn test_uri_with_partial_query() {
        let uri = "file:/home/user/db.sqlite?mode=rw";
        let opts = parse_sqlite_uri(uri).unwrap();
        assert_eq!(opts.path, "/home/user/db.sqlite");
        assert_eq!(opts.mode, Mode::ReadWrite);
        assert_eq!(opts.vfs, None);
    }

    #[test]
    fn test_uri_windows_style_path() {
        let uri = "file:///C:/Users/test/db.sqlite";
        let opts = parse_sqlite_uri(uri).unwrap();
        assert_eq!(opts.path, "/C:/Users/test/db.sqlite");
    }

    #[test]
    fn test_uri_with_only_query_params() {
        let uri = "file:?mode=memory&cache=shared";
        let opts = parse_sqlite_uri(uri).unwrap();
        assert_eq!(opts.path, "");
        assert_eq!(opts.mode, Mode::Memory);
        assert_eq!(opts.cache, Some(CacheMode::Shared));
    }

    #[test]
    fn test_uri_with_only_fragment() {
        let uri = "file:#fragment";
        let opts = parse_sqlite_uri(uri).unwrap();
        assert_eq!(opts.path, "");
        assert_eq!(opts.fragment, Some("fragment".to_string()));
    }

    #[test]
    fn test_uri_with_invalid_scheme() {
        let uri = "http:/home/user/db.sqlite";
        let result = parse_sqlite_uri(uri);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().path, "http:/home/user/db.sqlite");
    }

    #[test]
    fn test_uri_with_multiple_query_params() {
        let uri = "file:/home/user/db.sqlite?vfs=unix&mode=rw&cache=private&immutable=0";
        let opts = parse_sqlite_uri(uri).unwrap();
        assert_eq!(opts.path, "/home/user/db.sqlite");
        assert_eq!(opts.vfs, Some("unix".to_string()));
        assert_eq!(opts.mode, Mode::ReadWrite);
        assert_eq!(opts.cache, Some(CacheMode::Private));
        assert_eq!(opts.immutable, Some(false));
    }

    #[test]
    fn test_uri_with_unknown_query_param() {
        let uri = "file:/home/user/db.sqlite?unknown=param";
        let opts = parse_sqlite_uri(uri).unwrap();
        assert_eq!(opts.path, "/home/user/db.sqlite");
        assert_eq!(opts.vfs, None);
    }

    #[test]
    fn test_uri_with_multiple_equal_signs() {
        let uri = "file:/home/user/db.sqlite?vfs=unix=custom";
        let opts = parse_sqlite_uri(uri).unwrap();
        assert_eq!(opts.path, "/home/user/db.sqlite");
        assert_eq!(opts.vfs, Some("unix=custom".to_string()));
    }

    #[test]
    fn test_uri_with_trailing_slash() {
        let uri = "file:/home/user/db.sqlite/";
        let opts = parse_sqlite_uri(uri).unwrap();
        assert_eq!(opts.path, "/home/user/db.sqlite/");
    }

    #[test]
    fn test_uri_with_encoded_characters_in_query() {
        let uri = "file:/home/user/db.sqlite?vfs=unix%20mode";
        let opts = parse_sqlite_uri(uri).unwrap();
        assert_eq!(opts.path, "/home/user/db.sqlite");
        assert_eq!(opts.vfs, Some("unix mode".to_string()));
    }

    #[test]
    fn test_uri_windows_network_path() {
        let uri = "file://server/share/db.sqlite";
        let result = parse_sqlite_uri(uri);
        assert!(result.is_err()); // non-localhost authority should fail
    }

    #[test]
    fn test_uri_windows_drive_letter_with_slash() {
        let uri = "file:///C:/database.sqlite";
        let opts = parse_sqlite_uri(uri).unwrap();
        assert_eq!(opts.path, "/C:/database.sqlite");
    }

    #[test]
    fn test_localhost_with_double_slash_and_no_path() {
        let uri = "file://localhost";
        let opts = parse_sqlite_uri(uri).unwrap();
        assert_eq!(opts.path, "");
        assert_eq!(opts.authority, Some("localhost"));
    }

    #[test]
    fn test_uri_windows_drive_letter_without_slash() {
        let uri = "file:///C:/database.sqlite";
        let opts = parse_sqlite_uri(uri).unwrap();
        assert_eq!(opts.path, "/C:/database.sqlite");
    }

    #[test]
    fn test_improper_mode() {
        // any other mode but ro, rwc, rw, memory should fail per sqlite

        let uri = "file:data.db?mode=readonly";
        let res = parse_sqlite_uri(uri);
        assert!(res.is_err());
        // including empty
        let uri = "file:/home/user/db.sqlite?vfs=&mode=";
        let res = parse_sqlite_uri(uri);
        assert!(res.is_err());
    }

    // Some examples from https://www.sqlite.org/c3ref/open.html#urifilenameexamples
    #[test]
    fn test_simple_file_current_dir() {
        let uri = "file:data.db";
        let opts = parse_sqlite_uri(uri).unwrap();
        assert_eq!(opts.path, "data.db");
        assert_eq!(opts.authority, None);
        assert_eq!(opts.vfs, None);
        assert_eq!(opts.mode, Mode::ReadWriteCreate);
    }

    #[test]
    fn test_simple_file_three_slash() {
        let uri = "file:///home/data/data.db";
        let opts = parse_sqlite_uri(uri).unwrap();
        assert_eq!(opts.path, "/home/data/data.db");
        assert_eq!(opts.authority, None);
        assert_eq!(opts.vfs, None);
        assert_eq!(opts.mode, Mode::ReadWriteCreate);
    }

    #[test]
    fn test_simple_file_two_slash_localhost() {
        let uri = "file://localhost/home/fred/data.db";
        let opts = parse_sqlite_uri(uri).unwrap();
        assert_eq!(opts.path, "/home/fred/data.db");
        assert_eq!(opts.authority, Some("localhost"));
        assert_eq!(opts.vfs, None);
    }

    #[test]
    fn test_windows_double_invalid() {
        let uri = "file://C:/home/fred/data.db?mode=ro";
        let opts = parse_sqlite_uri(uri);
        assert!(opts.is_err());
    }

    #[test]
    fn test_simple_file_two_slash() {
        let uri = "file:///C:/Documents%20and%20Settings/fred/Desktop/data.db";
        let opts = parse_sqlite_uri(uri).unwrap();
        assert_eq!(opts.path, "/C:/Documents and Settings/fred/Desktop/data.db");
        assert_eq!(opts.vfs, None);
    }
}
