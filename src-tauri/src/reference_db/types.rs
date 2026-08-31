//! BG3 type system → SQLite type mapping and value coercion.

/// Map a BG3 type string to the SQLite column affinity.
pub fn sqlite_type(bg3_type: &str) -> &'static str {
    match bg3_type {
        "bool" | "int8" | "int16" | "int32" | "int64" | "uint8" | "uint16" | "uint32"
        | "uint64" => "INTEGER",
        "float" | "double" => "REAL",
        _ => "TEXT", // FixedString, LSString, string, guid, TranslatedString, fvecN, mat4x4, etc.
    }
}

/// Coerce a BG3 string value to a SQLite-compatible representation.
/// Returns the value as a `SqlValue` enum for binding.
#[derive(Debug, Clone)]
pub enum SqlValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
}

impl SqlValue {
    pub fn bind_to(&self, stmt: &mut rusqlite::Statement, idx: usize) -> rusqlite::Result<()> {
        match self {
            SqlValue::Null => stmt.raw_bind_parameter(idx, rusqlite::types::Null)?,
            SqlValue::Integer(n) => stmt.raw_bind_parameter(idx, n)?,
            SqlValue::Real(n) => stmt.raw_bind_parameter(idx, n)?,
            SqlValue::Text(s) => stmt.raw_bind_parameter(idx, s.as_str())?,
        }
        Ok(())
    }
}

impl rusqlite::types::ToSql for SqlValue {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        match self {
            SqlValue::Null => Ok(rusqlite::types::ToSqlOutput::Owned(
                rusqlite::types::Value::Null,
            )),
            SqlValue::Integer(n) => Ok(rusqlite::types::ToSqlOutput::Owned(
                rusqlite::types::Value::Integer(*n),
            )),
            SqlValue::Real(n) => Ok(rusqlite::types::ToSqlOutput::Owned(
                rusqlite::types::Value::Real(*n),
            )),
            SqlValue::Text(s) => Ok(rusqlite::types::ToSqlOutput::Borrowed(
                rusqlite::types::ValueRef::Text(s.as_bytes()),
            )),
        }
    }
}

/// Coerce a BG3 string value according to its type.
pub fn coerce_value(val: &str, bg3_type: &str) -> SqlValue {
    let st = sqlite_type(bg3_type);
    match st {
        "INTEGER" => {
            if bg3_type == "bool" {
                return SqlValue::Integer(if val.eq_ignore_ascii_case("true") || val == "1" {
                    1
                } else {
                    0
                });
            }
            // Try i64 parse
            if let Ok(n) = val.parse::<i64>() {
                SqlValue::Integer(n)
            } else {
                // uint64 values may exceed i64 range — store as text
                SqlValue::Text(val.to_string())
            }
        }
        "REAL" => {
            if let Ok(n) = val.parse::<f64>() {
                if n.is_finite() {
                    SqlValue::Real(n)
                } else {
                    SqlValue::Text(val.to_string())
                }
            } else {
                SqlValue::Text(val.to_string())
            }
        }
        _ => SqlValue::Text(val.to_string()),
    }
}

/// Coerce a BG3 value (owned String) — avoids allocation for TEXT types
/// by reusing the existing String.
pub fn coerce_value_owned(val: String, bg3_type: &str) -> SqlValue {
    let st = sqlite_type(bg3_type);
    match st {
        "INTEGER" => {
            if bg3_type == "bool" {
                return SqlValue::Integer(if val.eq_ignore_ascii_case("true") || val == "1" {
                    1
                } else {
                    0
                });
            }
            if let Ok(n) = val.parse::<i64>() {
                SqlValue::Integer(n)
            } else {
                SqlValue::Text(val)
            }
        }
        "REAL" => {
            if let Ok(n) = val.parse::<f64>() {
                if n.is_finite() {
                    SqlValue::Real(n)
                } else {
                    SqlValue::Text(val)
                }
            } else {
                SqlValue::Text(val)
            }
        }
        _ => SqlValue::Text(val),
    }
}
