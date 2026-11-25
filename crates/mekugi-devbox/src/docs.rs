use std::fs;
use std::path::Path;

use mekugi_transport::box_client::CodeModeDocs;

pub fn load_extracted_docs_file(path: &Path) -> Option<CodeModeDocs> {
    if !path.is_file() {
        return None;
    }
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn extract_rust_raw_str_const(source: &str, const_name: &str) -> Result<String, String> {
    for prefix in [
        format!("pub const {const_name}: &str = r"),
        format!("const {const_name}: &str = r"),
    ] {
        if let Some(pos) = source.find(&prefix) {
            let after = &source[pos + prefix.len()..];
            let hashes = after.bytes().take_while(|&b| b == b'#').count();
            let rest = &after[hashes..];
            let Some(body_start) = rest.strip_prefix('"') else {
                continue;
            };
            let closing = format!("\"{};", "#".repeat(hashes));
            let Some(end) = body_start.find(&closing) else {
                return Err(format!("Unterminated Rust raw string constant '{const_name}'"));
            };
            return Ok(body_start[..end].to_owned());
        }
    }
    Err(format!("Could not find Rust raw string constant '{const_name}'"))
}

pub fn parse_description_rs(
    source: &str,
    source_repo: &str,
    source_ref: &str,
) -> Result<CodeModeDocs, String> {
    let exec_tpl = extract_rust_raw_str_const(source, "EXEC_DESCRIPTION_TEMPLATE")?;
    let wait_tpl = extract_rust_raw_str_const(source, "WAIT_DESCRIPTION_TEMPLATE")?;
    let image_helper = extract_rust_raw_str_const(source, "UNIFIED_IMAGE_HELPER_DESCRIPTION")
        .or_else(|_| extract_rust_raw_str_const(source, "LEGACY_IMAGE_HELPER_DESCRIPTION"))?;
    Ok(CodeModeDocs {
        source_repo: source_repo.to_owned(),
        source_ref: source_ref.to_owned(),
        exec_description_template: exec_tpl,
        wait_description_template: wait_tpl,
        image_helper_description: image_helper,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_description_rs() {
        let sample = r###"
const UNIFIED_IMAGE_HELPER_DESCRIPTION: &str = r#"img helper"#;
const EXEC_DESCRIPTION_TEMPLATE: &str = r##"Run JS {{ default_exec_yield_time_ms }} "#ok"##;
const WAIT_DESCRIPTION_TEMPLATE: &str = r#"Wait for cell_id."#;
"###;
        let docs = parse_description_rs(sample, "mekugi-harness/mekugi", "abc").unwrap();
        assert_eq!(docs.source_ref, "abc");
        assert!(docs.exec_description_template.contains("\"#ok"));
        assert_eq!(docs.wait_description_template, "Wait for cell_id.");
    }
}
