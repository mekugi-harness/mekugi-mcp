use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RouteError {
    #[error("cell_id cannot be empty")]
    EmptyCellId,
    #[error(
        "conflicting box selection: cell_id '{cell_id}' belongs to box '{cell_box}', but box='{explicit_box}' was requested"
    )]
    ConflictingBox {
        cell_id: String,
        cell_box: String,
        explicit_box: String,
    },
}

/// Namespace a devbox-local cell_id with its box name so wait routes statelessly
/// across MCP server restarts.
pub fn format_cell_id(box_name: &str, raw_cell_id: &str) -> String {
    if raw_cell_id.is_empty() {
        return String::new();
    }
    if let Some((prefix, _)) = raw_cell_id.split_once(':')
        && prefix == box_name
    {
        return raw_cell_id.to_owned();
    }
    format!("{box_name}:{raw_cell_id}")
}

/// Resolve (target_box, raw_cell_id) from a potentially namespaced cell_id
/// and an optional explicit box parameter.
pub fn parse_cell_route(
    cell_id: &str,
    explicit_box: Option<&str>,
    default_box: &str,
) -> Result<(String, String), RouteError> {
    let trimmed_cell = cell_id.trim();
    if trimmed_cell.is_empty() {
        return Err(RouteError::EmptyCellId);
    }
    let explicit = explicit_box.map(str::trim).filter(|s| !s.is_empty());

    if let Some((prefix, raw)) = trimmed_cell.split_once(':')
        && !prefix.is_empty()
        && !raw.is_empty()
    {
        if let Some(exp) = explicit
            && exp != prefix
        {
            return Err(RouteError::ConflictingBox {
                cell_id: trimmed_cell.to_owned(),
                cell_box: prefix.to_owned(),
                explicit_box: exp.to_owned(),
            });
        }
        return Ok((prefix.to_owned(), raw.to_owned()));
    }

    let target_box = explicit.unwrap_or(default_box);
    Ok((target_box.to_owned(), trimmed_cell.to_owned()))
}

/// Rewrite raw cell_id occurrences inside Mekugi's known cell-ID anchors
/// ("cell ID <id>", "Cell <id> ", "cell_id='<id>'", "cell_id="<id>"")
/// so short/numeric IDs (e.g. "2") never corrupt timestamps or stdout.
pub fn rewrite_cell_id_in_text(text: &str, raw_cell_id: &str, namespaced_cell_id: &str) -> String {
    if raw_cell_id.is_empty() || raw_cell_id == namespaced_cell_id {
        return text.to_owned();
    }
    let mut result = text.to_owned();
    for anchor in ["cell ID ", "Cell ", "cell_id='", "cell_id=\""] {
        result = rewrite_after_anchor(&result, anchor, raw_cell_id, namespaced_cell_id);
    }
    result
}

fn is_cell_id_boundary(b: u8) -> bool {
    matches!(
        b,
        b' ' | b'\n' | b'\r' | b'\t' | b')' | b'\'' | b'"' | b'.' | b',' | b';'
    )
}

fn rewrite_after_anchor(
    text: &str,
    anchor: &str,
    raw_cell_id: &str,
    namespaced_cell_id: &str,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find(anchor) {
        let after_anchor = pos + anchor.len();
        out.push_str(&rest[..after_anchor]);
        rest = &rest[after_anchor..];

        if let Some(tail) = rest.strip_prefix(raw_cell_id) {
            let boundary_ok = tail
                .as_bytes()
                .first()
                .is_none_or(|b| is_cell_id_boundary(*b));
            if boundary_ok {
                out.push_str(namespaced_cell_id);
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}
