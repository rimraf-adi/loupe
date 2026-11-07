use crate::cache::{CacheStatus, SessionCache};
use serde::{Deserialize, Deserializer, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_MAX_TOKENS: usize = 12_000;
const CHARS_PER_TOKEN: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineRange {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum FileReadTarget {
    Detailed {
        path: PathBuf,
        #[serde(default)]
        ranges: Option<Vec<[usize; 2]>>,
    },
    Simple(PathBuf),
}

impl FileReadTarget {
    pub fn path(&self) -> &Path {
        match self {
            Self::Detailed { path, .. } => path,
            Self::Simple(p) => p,
        }
    }

    pub fn ranges(&self) -> Option<Vec<LineRange>> {
        match self {
            Self::Detailed { ranges, .. } => ranges.as_ref().map(|r| {
                r.iter()
                    .map(|&[s, e]| LineRange {
                        start: s.min(e),
                        end: s.max(e),
                    })
                    .collect()
            }),
            Self::Simple(_) => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReadFilesArgs {
    #[serde(deserialize_with = "deserialize_files")]
    pub files: Vec<FileReadTarget>,
    #[serde(default)]
    pub max_tokens: Option<usize>,
}

fn deserialize_files<'de, D>(deserializer: D) -> Result<Vec<FileReadTarget>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum FilesHelper {
        List(Vec<FileReadTarget>),
        Single(FileReadTarget),
    }

    match FilesHelper::deserialize(deserializer)? {
        FilesHelper::List(list) => Ok(list),
        FilesHelper::Single(item) => Ok(vec![item]),
    }
}

#[derive(Debug, Serialize)]
pub struct ReadFilesResult {
    pub text: String,
    pub tokens_estimated: usize,
    pub files_read: usize,
    pub files_unchanged: usize,
    pub truncated: bool,
}

pub fn execute_read_files(
    args: ReadFilesArgs,
    cache: &mut SessionCache,
    working_dir: Option<&Path>,
) -> ReadFilesResult {
    let max_tokens = args.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
    let mut remaining_chars = max_tokens.saturating_mul(CHARS_PER_TOKEN);
    let mut output = String::new();
    let mut files_read = 0;
    let mut files_unchanged = 0;
    let mut truncated = false;

    for target in &args.files {
        if remaining_chars == 0 {
            output.push_str("\n[Global token budget exhausted. Additional files skipped. Pass specific files or increase 'max_tokens'.]\n");
            truncated = true;
            break;
        }

        let raw_path = target.path();
        let effective_path = match working_dir {
            Some(wd) if !raw_path.is_absolute() => wd.join(raw_path),
            _ => raw_path.to_path_buf(),
        };

        if !effective_path.exists() {
            let err_header = format!(
                "=== File: {} [ERROR: File does not exist] ===\n\n",
                raw_path.display()
            );
            remaining_chars = remaining_chars.saturating_sub(err_header.len());
            output.push_str(&err_header);
            continue;
        }

        let content = match fs::read_to_string(&effective_path) {
            Ok(c) => c,
            Err(e) => {
                let err_header = format!(
                    "=== File: {} [ERROR: Could not read file: {}] ===\n\n",
                    raw_path.display(),
                    e
                );
                remaining_chars = remaining_chars.saturating_sub(err_header.len());
                output.push_str(&err_header);
                continue;
            }
        };

        let requested_ranges = target.ranges();
        let cache_status = cache.check_and_update(&effective_path, &content);

        // Deduplication: if whole file requested and it hasn't changed since last read in this session
        if requested_ranges.is_none() && matches!(cache_status, CacheStatus::Unchanged { .. }) {
            let hash = match cache_status {
                CacheStatus::Unchanged { hash } => hash,
                _ => unreachable!(),
            };
            let line_count = content.lines().count();
            let stub = format!(
                "=== File: {} [UNCHANGED in session, {} lines, hash: {}] ===\n[Content unchanged since prior read. Skipped full dump to avoid redundant round-trips. Specify 'ranges' to view any specific lines.]\n\n",
                raw_path.display(),
                line_count,
                &hash[..8.min(hash.len())]
            );
            remaining_chars = remaining_chars.saturating_sub(stub.len());
            output.push_str(&stub);
            files_unchanged += 1;
            continue;
        }

        files_read += 1;
        let hash_str = match &cache_status {
            CacheStatus::Unchanged { hash } | CacheStatus::ModifiedOrNew { hash } => {
                &hash[..8.min(hash.len())]
            }
        };

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();
        let line_num_width = format!("{}", total_lines).len().max(3);

        if let Some(mut ranges) = requested_ranges {
            // Normalize & merge ranges
            ranges = normalize_ranges(ranges, total_lines);

            let header = format!(
                "=== File: {} ({} selected ranges of {} lines, hash: {}) ===\n",
                raw_path.display(),
                ranges.len(),
                total_lines,
                hash_str
            );
            remaining_chars = remaining_chars.saturating_sub(header.len());
            output.push_str(&header);

            let mut prev_end = 0;
            for r in ranges {
                if r.start > prev_end + 1 {
                    let skipped = r.start - prev_end - 1;
                    let skip_marker = format!(
                        "  [... {} lines omitted (L{}-L{}) ...]\n",
                        skipped,
                        prev_end + 1,
                        r.start - 1
                    );
                    remaining_chars = remaining_chars.saturating_sub(skip_marker.len());
                    output.push_str(&skip_marker);
                }

                for line_idx in r.start..=r.end {
                    let line_text = lines.get(line_idx - 1).unwrap_or(&"");
                    let formatted = format!(
                        "{:>width$} | {}\n",
                        line_idx,
                        line_text,
                        width = line_num_width
                    );
                    if remaining_chars < formatted.len() {
                        output.push_str(&format!(
                            "  [Token budget limit reached for {}. Omitted remaining requested lines.]\n",
                            raw_path.display()
                        ));
                        truncated = true;
                        remaining_chars = 0;
                        break;
                    }
                    remaining_chars -= formatted.len();
                    output.push_str(&formatted);
                }
                prev_end = r.end;
                if remaining_chars == 0 {
                    break;
                }
            }

            if prev_end < total_lines && remaining_chars > 0 {
                let remaining_tail = total_lines - prev_end;
                let tail_marker = format!(
                    "  [... {} lines to EOF (L{}-L{}) ...]\n",
                    remaining_tail,
                    prev_end + 1,
                    total_lines
                );
                remaining_chars = remaining_chars.saturating_sub(tail_marker.len());
                output.push_str(&tail_marker);
            }
            output.push('\n');
        } else {
            // Whole file requested
            let header = format!(
                "=== File: {} ({} lines, hash: {}) ===\n",
                raw_path.display(),
                total_lines,
                hash_str
            );
            remaining_chars = remaining_chars.saturating_sub(header.len());
            output.push_str(&header);

            let mut lines_emitted = 0;
            for (idx, line_text) in lines.iter().enumerate() {
                let line_idx = idx + 1;
                let formatted = format!(
                    "{:>width$} | {}\n",
                    line_idx,
                    line_text,
                    width = line_num_width
                );
                if remaining_chars < formatted.len() {
                    let _omitted = total_lines - lines_emitted;
                    output.push_str(&format!(
                        "  [Budget limit reached (~{} tokens). Showing first {} of {} lines. Pass 'ranges' to inspect L{}-L{}.]\n",
                        max_tokens, lines_emitted, total_lines, line_idx, total_lines
                    ));
                    truncated = true;
                    remaining_chars = 0;
                    break;
                }
                remaining_chars -= formatted.len();
                output.push_str(&formatted);
                lines_emitted += 1;
            }
            output.push('\n');
        }
    }

    let estimated_tokens = (output.len() + CHARS_PER_TOKEN - 1) / CHARS_PER_TOKEN;

    ReadFilesResult {
        text: output,
        tokens_estimated: estimated_tokens,
        files_read,
        files_unchanged,
        truncated,
    }
}

fn normalize_ranges(mut ranges: Vec<LineRange>, max_line: usize) -> Vec<LineRange> {
    if max_line == 0 || ranges.is_empty() {
        return vec![];
    }

    // Clamp to valid 1..=max_line range
    for r in &mut ranges {
        r.start = r.start.max(1).min(max_line);
        r.end = r.end.max(1).min(max_line);
        if r.start > r.end {
            std::mem::swap(&mut r.start, &mut r.end);
        }
    }

    ranges.sort_by_key(|r| r.start);

    let mut merged: Vec<LineRange> = Vec::new();
    for r in ranges {
        if let Some(last) = merged.last_mut() {
            if r.start <= last.end + 1 {
                last.end = last.end.max(r.end);
                continue;
            }
        }
        merged.push(r);
    }

    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_normalize_ranges() {
        let r = vec![
            LineRange { start: 10, end: 20 },
            LineRange { start: 15, end: 25 },
            LineRange { start: 30, end: 40 },
        ];
        let normalized = normalize_ranges(r, 100);
        assert_eq!(normalized.len(), 2);
        assert_eq!(normalized[0], LineRange { start: 10, end: 25 });
        assert_eq!(normalized[1], LineRange { start: 30, end: 40 });
    }

    #[test]
    fn test_read_files_whole_and_dedup() {
        let mut tmp = NamedTempFile::new().unwrap();
        writeln!(tmp, "fn main() {{\n    println!(\"hello\");\n}}").unwrap();
        let path = tmp.path().to_path_buf();

        let mut cache = SessionCache::new();
        let args = ReadFilesArgs {
            files: vec![FileReadTarget::Simple(path.clone())],
            max_tokens: Some(1000),
        };

        let res1 = execute_read_files(args.clone(), &mut cache, None);
        assert_eq!(res1.files_read, 1);
        assert_eq!(res1.files_unchanged, 0);
        assert!(res1.text.contains("fn main()"));
        assert!(res1.text.contains("1 | fn main()"));

        // Second read of identical file without ranges -> deduplication stub
        let res2 = execute_read_files(args, &mut cache, None);
        assert_eq!(res2.files_read, 0);
        assert_eq!(res2.files_unchanged, 1);
        assert!(res2.text.contains("UNCHANGED in session"));
    }
}
