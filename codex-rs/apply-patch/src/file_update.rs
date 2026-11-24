use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::FileSystemSandboxContext;
use codex_exec_server::ReadFileOptions;
use codex_utils_path_uri::PathUri;
use similar::TextDiff;

use crate::ApplyPatchError;
use crate::IoError;
use crate::UpdateFileChunk;
use crate::seek_sequence;
use crate::seek_sequence::MatchLevel;
use crate::seek_sequence::SequenceMatch;
use crate::text_file::Replacement;
use crate::text_file::SourceFile;

#[cfg(test)]
#[path = "file_update_tests.rs"]
mod tests;

pub(crate) struct AppliedPatch {
    pub(crate) original_contents: String,
    pub(crate) new_contents: String,
    pub(crate) fuzzy_matches: Vec<String>,
}

/// Return *only* the new file contents (joined into a single `String`) after
/// applying the chunks to the file at `path`.
pub(crate) async fn derive_new_contents_from_chunks(
    path: &PathUri,
    chunks: &[UpdateFileChunk],
    fs: &dyn ExecutorFileSystem,
    follow_symlinks: bool,
    sandbox: Option<&FileSystemSandboxContext>,
) -> std::result::Result<AppliedPatch, ApplyPatchError> {
    let original_contents = fs
        .read_file_text(path, ReadFileOptions { follow_symlinks }, sandbox)
        .await
        .map_err(|err| {
            ApplyPatchError::IoError(IoError {
                context: format!(
                    "Failed to read file to update {}",
                    path.inferred_native_path_string()
                ),
                source: err,
            })
        })?;

    let path_text = path.inferred_native_path_string();
    let mut source_file = SourceFile::parse(&original_contents);
    let original_lines = source_file.line_texts();
    let (replacements, fuzzy_matches) = compute_replacements(&original_lines, &path_text, chunks)?;
    source_file.apply_replacements(&replacements);
    let new_contents = source_file.into_contents();
    Ok(AppliedPatch {
        original_contents,
        new_contents,
        fuzzy_matches,
    })
}

/// Compute a list of replacements needed to transform `original_lines` into the
/// new lines, given the patch `chunks`. Each replacement is returned as
/// `(start_index, old_len, new_lines)`.
fn compute_replacements(
    original_lines: &[String],
    path: &str,
    chunks: &[UpdateFileChunk],
) -> std::result::Result<(Vec<Replacement>, Vec<String>), ApplyPatchError> {
    let mut replacements: Vec<Replacement> = Vec::new();
    let mut fuzzy_matches = Vec::new();
    let mut line_index: usize = 0;

    for chunk in chunks {
        // If a chunk has a `change_context`, we use seek_sequence to find it, then
        // adjust our `line_index` to continue from there.
        if let Some(ctx_line) = &chunk.change_context {
            if let Some(sequence_match) = seek_sequence::seek_sequence(
                original_lines,
                std::slice::from_ref(ctx_line),
                line_index,
                /*eof*/ false,
            ) {
                note_fuzzy_match(path, &sequence_match, &mut fuzzy_matches);
                line_index = sequence_match.index + 1;
            } else {
                return Err(ApplyPatchError::ComputeReplacements(
                    missing_sequence_error(
                        original_lines,
                        path,
                        std::slice::from_ref(ctx_line),
                        line_index,
                        "context",
                    ),
                ));
            }
        }

        if chunk.old_lines.is_empty() {
            replacements.push((original_lines.len(), 0, chunk.new_lines.clone()));
            continue;
        }

        // Otherwise, try to match the existing lines in the file with the old lines
        // from the chunk. If found, schedule that region for replacement.
        // Attempt to locate the `old_lines` verbatim within the file.  In many
        // real‑world diffs the last element of `old_lines` is an *empty* string
        // representing the terminating newline of the region being replaced.
        // This sentinel is not present in `original_lines` because `SourceFile`
        // stores the terminator on the preceding line rather than as an extra
        // trailing element. If a direct search fails and the pattern ends with
        // an empty string, retry without that final element so modifications
        // touching the end‑of‑file can be located reliably.

        let mut pattern: &[String] = &chunk.old_lines;
        let mut found =
            seek_sequence::seek_sequence(original_lines, pattern, line_index, chunk.is_end_of_file);

        let mut new_slice: &[String] = &chunk.new_lines;

        if found.is_none() && pattern.last().is_some_and(String::is_empty) {
            // Retry without the trailing empty line which represents the final
            // newline in the file.
            pattern = &pattern[..pattern.len() - 1];
            if new_slice.last().is_some_and(String::is_empty) {
                new_slice = &new_slice[..new_slice.len() - 1];
            }

            found = seek_sequence::seek_sequence(
                original_lines,
                pattern,
                line_index,
                chunk.is_end_of_file,
            );
        }

        if let Some(sequence_match) = found {
            if sequence_match.level != MatchLevel::Exact && sequence_match.match_count > 1 {
                return Err(ApplyPatchError::ComputeReplacements(format!(
                    "Ambiguous patch context in {path}: the expected block matched {} locations using {} matching, first at line {}. Include more surrounding lines in `@@` / the update block, then retry.",
                    sequence_match.match_count,
                    sequence_match.level,
                    sequence_match.index + 1,
                )));
            }
            note_fuzzy_match(path, &sequence_match, &mut fuzzy_matches);
            let start_idx = sequence_match.index;
            // Context lines occur in both sides of a patch chunk. Keep those
            // original lines in place so their exact contents and terminators
            // survive, especially when the file has mixed line endings.
            let mut old_start = 0;
            let mut new_start = 0;
            for &(old_context, new_context) in &chunk.context_line_indices {
                // A trailing empty context line can be removed from `pattern`
                // and `new_slice` above when it represents the final newline.
                if old_context >= pattern.len() || new_context >= new_slice.len() {
                    break;
                }
                if old_start != old_context || new_start != new_context {
                    replacements.push((
                        start_idx + old_start,
                        old_context - old_start,
                        new_slice[new_start..new_context].to_vec(),
                    ));
                }
                old_start = old_context + 1;
                new_start = new_context + 1;
            }
            if old_start != pattern.len() || new_start != new_slice.len() {
                replacements.push((
                    start_idx + old_start,
                    pattern.len() - old_start,
                    new_slice[new_start..].to_vec(),
                ));
            }
            line_index = start_idx + pattern.len();
        } else {
            return Err(ApplyPatchError::ComputeReplacements(
                missing_sequence_error(original_lines, path, pattern, line_index, "expected lines"),
            ));
        }
    }

    replacements.sort_by_key(|(index, _, _)| *index);

    Ok((replacements, fuzzy_matches))
}

fn note_fuzzy_match(path: &str, sequence_match: &SequenceMatch, notices: &mut Vec<String>) {
    if sequence_match.level != MatchLevel::Exact {
        notices.push(format!(
            "Applied update to {path} at line {} using {} matching.",
            sequence_match.index + 1,
            sequence_match.level
        ));
    }
}

fn missing_sequence_error(
    original_lines: &[String],
    path: &str,
    expected: &[String],
    search_start: usize,
    label: &str,
) -> String {
    let anchor = expected
        .iter()
        .find(|line| !line.trim().is_empty())
        .or_else(|| expected.first())
        .map(String::as_str)
        .unwrap_or("");
    let nearest = (search_start..original_lines.len())
        .max_by_key(|index| line_similarity(anchor, &original_lines[*index]))
        .unwrap_or(search_start.min(original_lines.len()));
    let window_start = nearest.saturating_sub(5);
    let window_end = (nearest + 6).min(original_lines.len());
    let actual = original_lines[window_start..window_end]
        .iter()
        .enumerate()
        .map(|(offset, line)| format!("{:>5} | {line}", window_start + offset + 1))
        .collect::<Vec<_>>()
        .join("\n");
    let expected = expected
        .iter()
        .enumerate()
        .map(|(offset, line)| format!("{:>5} | {line}", offset + 1))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Failed to find {label} in {path}.\n\
         Tried exact, trailing-whitespace-insensitive, whitespace-insensitive, and Unicode-normalized matching from line {}.\n\
         Expected patch text:\n{expected}\n\
         Actual file near line {}:\n{actual}\n\
         Next step: re-read {path}, then rebuild the update with current surrounding lines.",
        search_start + 1,
        nearest + 1,
    )
}

fn line_similarity(left: &str, right: &str) -> usize {
    let left = left.trim();
    let right = right.trim();
    if left == right {
        return usize::MAX;
    }
    let shared_prefix = left
        .chars()
        .zip(right.chars())
        .take_while(|(left, right)| left == right)
        .count();
    let containment_bonus = usize::from(!left.is_empty() && right.contains(left)) * left.len();
    shared_prefix + containment_bonus
}

/// Intended result of a file update for apply_patch.
#[derive(Debug, Eq, PartialEq)]
pub struct ApplyPatchFileUpdate {
    pub(crate) unified_diff: String,
    pub(crate) original_content: String,
    pub(crate) content: String,
}

pub async fn unified_diff_from_chunks(
    path: &PathUri,
    chunks: &[UpdateFileChunk],
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> std::result::Result<ApplyPatchFileUpdate, ApplyPatchError> {
    unified_diff_from_chunks_with_context(path, chunks, /*context*/ 1, fs, sandbox).await
}

pub async fn unified_diff_from_chunks_with_context(
    path: &PathUri,
    chunks: &[UpdateFileChunk],
    context: usize,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> std::result::Result<ApplyPatchFileUpdate, ApplyPatchError> {
    let AppliedPatch {
        original_contents,
        new_contents,
        ..
    } = derive_new_contents_from_chunks(path, chunks, fs, /*follow_symlinks*/ true, sandbox)
        .await?;
    let text_diff = TextDiff::from_lines(&original_contents, &new_contents);
    let unified_diff = text_diff.unified_diff().context_radius(context).to_string();
    Ok(ApplyPatchFileUpdate {
        unified_diff,
        original_content: original_contents,
        content: new_contents,
    })
}
