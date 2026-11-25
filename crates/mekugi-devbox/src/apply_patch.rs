use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
#[error("{0}")]
pub struct PatchError(pub String);

#[derive(Debug, Clone)]
struct PatchHunk {
    context_header: Option<String>,
    old_lines: Vec<String>,
    new_lines: Vec<String>,
    end_of_file: bool,
}

#[derive(Debug, Clone)]
enum PatchOp {
    Add {
        rel: String,
        target: PathBuf,
        content: String,
    },
    Delete {
        rel: String,
        target: PathBuf,
    },
    Update {
        rel: String,
        source: PathBuf,
        move_rel: Option<String>,
        move_to: Option<PathBuf>,
        hunks: Vec<PatchHunk>,
    },
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn resolve_path(path_str: &str, base: &Path) -> PathBuf {
    let p = Path::new(path_str.trim());
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    normalize_path(&joined)
}

fn lines_equal(left: &[String], right: &[String], strip_trailing: bool) -> bool {
    if left.len() != right.len() {
        return false;
    }
    if !strip_trailing {
        return left == right;
    }
    left.iter()
        .zip(right.iter())
        .all(|(l, r)| l.trim_end() == r.trim_end())
}

fn find_subsequence(
    lines: &[String],
    pattern: &[String],
    start: usize,
    eof: bool,
) -> Option<usize> {
    if pattern.is_empty() {
        return Some(lines.len());
    }
    let n = pattern.len();
    if eof {
        if lines.len() < n {
            return None;
        }
        let pos = lines.len() - n;
        if pos < start {
            return None;
        }
        if lines_equal(&lines[pos..], pattern, false)
            || lines_equal(&lines[pos..], pattern, true)
        {
            return Some(pos);
        }
        return None;
    }
    if start + n > lines.len() {
        return None;
    }
    for idx in start..=(lines.len() - n) {
        if lines_equal(&lines[idx..idx + n], pattern, false) {
            return Some(idx);
        }
    }
    for idx in start..=(lines.len() - n) {
        if lines_equal(&lines[idx..idx + n], pattern, true) {
            return Some(idx);
        }
    }
    None
}

fn split_lines_exact(s: &str) -> Vec<String> {
    s.lines().map(ToOwned::to_owned).collect()
}

fn apply_hunks(original: &str, hunks: &[PatchHunk], file_label: &str) -> Result<String, PatchError> {
    let newline = if original.contains("\r\n") { "\r\n" } else { "\n" };
    let had_trailing_nl = original.ends_with("\r\n") || original.ends_with('\n');
    let mut lines = split_lines_exact(original);
    let mut cursor = 0usize;

    for hunk in hunks {
        let mut search_start = cursor;
        let mut context_found_at: Option<usize> = None;
        if let Some(header) = &hunk.context_header {
            for (idx, line) in lines.iter().enumerate().skip(cursor) {
                if line.contains(header.as_str()) {
                    context_found_at = Some(idx);
                    search_start = idx;
                    break;
                }
            }
            if context_found_at.is_none() {
                return Err(PatchError(format!(
                    "Failed to find context header '{header}' in {file_label}"
                )));
            }
        }

        if hunk.old_lines.is_empty() {
            let insert_pos = match context_found_at {
                Some(idx) if !hunk.end_of_file => idx + 1,
                _ => lines.len(),
            };
            lines.splice(insert_pos..insert_pos, hunk.new_lines.iter().cloned());
            cursor = insert_pos + hunk.new_lines.len();
            continue;
        }

        let mut pos = find_subsequence(&lines, &hunk.old_lines, search_start, hunk.end_of_file);
        if pos.is_none() && hunk.context_header.is_none() && !hunk.end_of_file && search_start > 0
        {
            pos = find_subsequence(&lines, &hunk.old_lines, 0, false);
        }
        let Some(pos) = pos else {
            return Err(PatchError(format!("Failed to find context in {file_label}")));
        };

        lines.splice(pos..pos + hunk.old_lines.len(), hunk.new_lines.iter().cloned());
        cursor = pos + hunk.new_lines.len();
    }

    let mut result = lines.join(newline);
    if !lines.is_empty() && (had_trailing_nl || original.is_empty()) {
        result.push_str(newline);
    }
    Ok(result)
}

fn parse_patch(patch_text: &str, base: &Path) -> Result<Vec<PatchOp>, PatchError> {
    let raw_lines: Vec<&str> = patch_text.lines().collect();
    let mut idx = 0usize;

    while idx < raw_lines.len() && !raw_lines[idx].starts_with("*** Begin Patch") {
        idx += 1;
    }
    if idx >= raw_lines.len() {
        return Err(PatchError("Missing '*** Begin Patch' header".into()));
    }
    idx += 1;

    let mut ops = Vec::new();
    let mut found_end = false;

    while idx < raw_lines.len() {
        let line = raw_lines[idx];
        if line.starts_with("*** End Patch") {
            found_end = true;
            break;
        }

        if let Some(rel_raw) = line.strip_prefix("*** Add File: ") {
            let rel = rel_raw.trim().to_owned();
            let target = resolve_path(&rel, base);
            idx += 1;
            let mut added = Vec::new();
            while idx < raw_lines.len() && !raw_lines[idx].starts_with("*** ") {
                let cur = raw_lines[idx];
                let Some(body) = cur.strip_prefix('+') else {
                    return Err(PatchError(format!(
                        "Expected '+' line in Add File for {rel}: {cur:?}"
                    )));
                };
                added.push(body);
                idx += 1;
            }
            let mut content = added.join("\n");
            if !added.is_empty() {
                content.push('\n');
            }
            ops.push(PatchOp::Add {
                rel,
                target,
                content,
            });
            continue;
        }

        if let Some(rel_raw) = line.strip_prefix("*** Delete File: ") {
            let rel = rel_raw.trim().to_owned();
            let target = resolve_path(&rel, base);
            ops.push(PatchOp::Delete { rel, target });
            idx += 1;
            continue;
        }

        if let Some(rel_raw) = line.strip_prefix("*** Update File: ") {
            let rel = rel_raw.trim().to_owned();
            let source = resolve_path(&rel, base);
            idx += 1;

            let mut move_rel = None;
            let mut move_to = None;
            if idx < raw_lines.len()
                && let Some(mraw) = raw_lines[idx].strip_prefix("*** Move to: ")
            {
                let m = mraw.trim().to_owned();
                move_to = Some(resolve_path(&m, base));
                move_rel = Some(m);
                idx += 1;
            }

            let mut hunks = Vec::new();
            while idx < raw_lines.len() && !raw_lines[idx].starts_with("*** ") {
                let cur = raw_lines[idx];
                let Some(rest_header) = cur.strip_prefix("@@") else {
                    return Err(PatchError(format!(
                        "Expected '@@' hunk header in {rel}, got: {cur:?}"
                    )));
                };
                let header = {
                    let t = rest_header.trim();
                    if t.is_empty() { None } else { Some(t.to_owned()) }
                };
                idx += 1;

                let mut old_lines = Vec::new();
                let mut new_lines = Vec::new();
                let mut eof = false;

                while idx < raw_lines.len() {
                    let hline = raw_lines[idx];
                    if hline.starts_with("@@") || hline.starts_with("*** ") {
                        if hline.starts_with("*** End of File") {
                            eof = true;
                            idx += 1;
                        }
                        break;
                    }
                    if let Some(body) = hline.strip_prefix(' ') {
                        old_lines.push(body.to_owned());
                        new_lines.push(body.to_owned());
                    } else if let Some(body) = hline.strip_prefix('-') {
                        old_lines.push(body.to_owned());
                    } else if let Some(body) = hline.strip_prefix('+') {
                        new_lines.push(body.to_owned());
                    } else if hline.is_empty() {
                        old_lines.push(String::new());
                        new_lines.push(String::new());
                    } else {
                        return Err(PatchError(format!(
                            "Unexpected hunk line in {rel}: {hline:?}"
                        )));
                    }
                    idx += 1;
                }

                hunks.push(PatchHunk {
                    context_header: header,
                    old_lines,
                    new_lines,
                    end_of_file: eof,
                });
            }

            ops.push(PatchOp::Update {
                rel,
                source,
                move_rel,
                move_to,
                hunks,
            });
            continue;
        }

        return Err(PatchError(format!("Unexpected patch directive: {line:?}")));
    }

    if !found_end {
        return Err(PatchError("Missing '*** End Patch' footer".into()));
    }
    Ok(ops)
}

fn write_exact(path: &Path, content: &str, mode_override: Option<u32>) -> Result<(), PatchError> {
    let parent = path
        .parent()
        .ok_or_else(|| PatchError(format!("Invalid target path: {}", path.display())))?;
    fs::create_dir_all(parent)
        .map_err(|e| PatchError(format!("Failed to create directory {}: {e}", parent.display())))?;

    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        mode_override.unwrap_or_else(|| {
            fs::metadata(path)
                .ok()
                .map(|m| m.permissions().mode() & 0o7777)
                .unwrap_or(0o644)
        })
    };
    #[cfg(not(unix))]
    let _ = mode_override;

    let tmp_path = parent.join(format!(".mekugi-patch-{}", Uuid::new_v4().simple()));
    let write_res = (|| -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp_path)?;
        f.write_all(content.as_bytes())?;
        f.flush()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp_path, fs::Permissions::from_mode(mode))?;
        }
        fs::rename(&tmp_path, path)?;
        Ok(())
    })();

    if let Err(e) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(PatchError(format!("Failed to write {}: {e}", path.display())));
    }
    Ok(())
}

pub fn apply_patch(patch_text: &str, workdir: &Path) -> Result<String, PatchError> {
    let base = fs::canonicalize(workdir).unwrap_or_else(|_| normalize_path(workdir));
    let ops = parse_patch(patch_text, &base)?;

    // Stage all edits in memory before touching disk so malformed/truncated patches leave no partial writes.
    let mut staged: BTreeMap<PathBuf, Option<String>> = BTreeMap::new();
    let mut staged_modes: BTreeMap<PathBuf, u32> = BTreeMap::new();
    let mut summary = Vec::new();

    for op in ops {
        match op {
            PatchOp::Add {
                rel,
                target,
                content,
            } => {
                staged.insert(target, Some(content));
                summary.push(format!("A {rel}"));
            }
            PatchOp::Delete { rel, target } => {
                let exists = match staged.get(&target) {
                    Some(opt) => opt.is_some(),
                    None => target.is_file(),
                };
                if !exists {
                    return Err(PatchError(format!("Cannot delete missing file: {rel}")));
                }
                staged.insert(target, None);
                summary.push(format!("D {rel}"));
            }
            PatchOp::Update {
                rel,
                source,
                move_rel,
                move_to,
                hunks,
            } => {
                let original = match staged.get(&source) {
                    Some(Some(s)) => s.clone(),
                    Some(None) => {
                        return Err(PatchError(format!("Cannot update missing file: {rel}")));
                    }
                    None => {
                        if !source.is_file() {
                            return Err(PatchError(format!("Cannot update missing file: {rel}")));
                        }
                        fs::read_to_string(&source).map_err(|e| {
                            PatchError(format!("Failed to read {rel}: {e}"))
                        })?
                    }
                };
                let updated = if hunks.is_empty() {
                    original
                } else {
                    apply_hunks(&original, &hunks, &rel)?
                };
                if let (Some(mto), Some(mrel)) = (move_to, move_rel) {
                    #[cfg(unix)]
                    if let Ok(meta) = fs::metadata(&source) {
                        use std::os::unix::fs::PermissionsExt;
                        staged_modes.insert(mto.clone(), meta.permissions().mode() & 0o7777);
                    }
                    if mto != source {
                        staged.insert(source, None);
                    }
                    staged.insert(mto, Some(updated));
                    summary.push(format!("M {rel} -> {mrel}"));
                } else {
                    staged.insert(source, Some(updated));
                    summary.push(format!("M {rel}"));
                }
            }
        }
    }

    let deletes: HashSet<PathBuf> = staged
        .iter()
        .filter(|(_, c)| c.is_none())
        .map(|(p, _)| p.clone())
        .collect();
    let writes: BTreeMap<PathBuf, String> = staged
        .into_iter()
        .filter_map(|(p, c)| c.map(|s| (p, s)))
        .collect();

    let mut blocking_deletes = HashSet::new();
    for path in writes.keys() {
        if path.is_dir() {
            return Err(PatchError(format!(
                "Cannot overwrite directory with file: {}",
                path.display()
            )));
        }
        let mut cur = path.parent();
        while let Some(parent) = cur {
            if parent == base || parent.as_os_str().is_empty() {
                break;
            }
            if writes.contains_key(parent) {
                return Err(PatchError(format!(
                    "Conflicting patch paths (file and directory): {}",
                    parent.display()
                )));
            }
            if parent.is_file() {
                if deletes.contains(parent) {
                    blocking_deletes.insert(parent.to_path_buf());
                } else {
                    return Err(PatchError(format!(
                        "Cannot create directory over existing file: {}",
                        parent.display()
                    )));
                }
            }
            cur = parent.parent();
        }
    }

    for path in &blocking_deletes {
        if path.exists() {
            fs::remove_file(path)
                .map_err(|e| PatchError(format!("Failed to delete {}: {e}", path.display())))?;
        }
    }
    for (path, content) in &writes {
        write_exact(path, content, staged_modes.get(path).copied())?;
    }
    for path in deletes.difference(&blocking_deletes) {
        if path.exists() {
            fs::remove_file(path)
                .map_err(|e| PatchError(format!("Failed to delete {}: {e}", path.display())))?;
        }
    }

    if summary.is_empty() {
        Ok("Success. No files changed.".into())
    } else {
        Ok(format!(
            "Success. Updated the following files:
{}",
            summary.join("
")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_add_update_move_delete_and_crlf() {
        let tmp = TempDir::new().unwrap();
        let res = apply_patch(
            "*** Begin Patch\n*** Add File: hello.txt\n+line 1\n+line 2\n*** End Patch",
            tmp.path(),
        )
        .unwrap();
        assert!(res.contains("A hello.txt"));
        assert_eq!(
            fs::read_to_string(tmp.path().join("hello.txt")).unwrap(),
            "line 1\nline 2\n"
        );

        let res2 = apply_patch(
            "*** Begin Patch\n*** Update File: hello.txt\n*** Move to: sub/greet.txt\n@@\n line 1\n-line 2\n+line two\n*** End Patch",
            tmp.path(),
        )
        .unwrap();
        assert!(res2.contains("M hello.txt -> sub/greet.txt"));
        assert!(!tmp.path().join("hello.txt").exists());
        assert_eq!(
            fs::read_to_string(tmp.path().join("sub/greet.txt")).unwrap(),
            "line 1\nline two\n"
        );

        // CRLF preservation
        fs::write(tmp.path().join("win.txt"), "a\r\nb\r\n").unwrap();
        apply_patch(
            "*** Begin Patch\n*** Update File: win.txt\n@@\n a\n-b\n+c\n*** End Patch",
            tmp.path(),
        )
        .unwrap();
        assert_eq!(
            fs::read(tmp.path().join("win.txt")).unwrap(),
            b"a\r\nc\r\n"
        );

        // Truncated patch commits nothing
        let err = apply_patch(
            "*** Begin Patch\n*** Add File: partial.txt\n+should not exist\n",
            tmp.path(),
        );
        assert!(err.is_err());
        assert!(!tmp.path().join("partial.txt").exists());
    }
}
