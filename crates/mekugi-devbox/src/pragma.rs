use serde::Deserialize;
use thiserror::Error;

pub const CODE_MODE_PRAGMA_PREFIX: &str = "// @exec:";

#[derive(Debug, Error)]
pub enum PragmaError {
    #[error("exec expects raw JavaScript source text (non-empty).")]
    EmptySource,
    #[error("Invalid // @exec pragma JSON: {0}")]
    InvalidJson(String),
    #[error("Invalid // @exec pragma: {0}")]
    InvalidField(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedExecSource {
    pub code: String,
    pub yield_time_ms: Option<u32>,
    pub max_output_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct RawPragma {
    #[serde(default)]
    yield_time_ms: Option<i64>,
    #[serde(default)]
    max_output_tokens: Option<i64>,
}

pub fn parse_exec_source(raw: &str) -> Result<ParsedExecSource, PragmaError> {
    if raw.trim().is_empty() {
        return Err(PragmaError::EmptySource);
    }

    let (first_line, rest) = match raw.split_once('\n') {
        Some((first, r)) => (first, r),
        None => (raw, ""),
    };

    let trimmed = first_line.trim_start();
    let Some(json_part) = trimmed.strip_prefix(CODE_MODE_PRAGMA_PREFIX) else {
        return Ok(ParsedExecSource {
            code: raw.to_owned(),
            yield_time_ms: None,
            max_output_tokens: None,
        });
    };

    let parsed: RawPragma = serde_json::from_str(json_part.trim())
        .map_err(|e| PragmaError::InvalidJson(e.to_string()))?;

    let yield_time_ms = match parsed.yield_time_ms {
        Some(v) if v < 0 => {
            return Err(PragmaError::InvalidField(
                "yield_time_ms must be non-negative".into(),
            ));
        }
        Some(v) => Some(v.min(i64::from(u32::MAX)) as u32),
        None => None,
    };

    let max_output_tokens = match parsed.max_output_tokens {
        Some(v) if v <= 0 => {
            return Err(PragmaError::InvalidField(
                "max_output_tokens must be positive".into(),
            ));
        }
        Some(v) => Some(v.min(i64::from(u32::MAX)) as u32),
        None => None,
    };

    if rest.trim().is_empty() {
        return Err(PragmaError::EmptySource);
    }

    Ok(ParsedExecSource {
        code: rest.to_owned(),
        yield_time_ms,
        max_output_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_and_pragma() {
        let plain = parse_exec_source("text('hi');").unwrap();
        assert_eq!(plain.code, "text('hi');");
        assert_eq!(plain.yield_time_ms, None);
        assert_eq!(plain.max_output_tokens, None);

        let with_p = parse_exec_source(
            "// @exec: {\"yield_time_ms\": 5000, \"max_output_tokens\": 2048}\ntext(\"ok\");",
        )
        .unwrap();
        assert_eq!(with_p.code, "text(\"ok\");");
        assert_eq!(with_p.yield_time_ms, Some(5000));
        assert_eq!(with_p.max_output_tokens, Some(2048));

        assert!(parse_exec_source("// @exec: {bad json\ntext('x');").is_err());
    }
}
