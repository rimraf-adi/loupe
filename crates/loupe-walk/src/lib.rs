//! File discovery, ignore rules, language detection, hashing, and snapshot diffing.

mod diff;
mod exclude;
mod lang;
mod scan;
mod secrets;

pub use diff::{Diff, diff};
pub use exclude::{has_skipped_extension, is_excluded_dir, is_in_excluded_dir};
pub use lang::detect_language;
pub use scan::{Scan, WalkOptions, WalkStats, is_indexable_content, is_stale, scan};
pub use secrets::is_secret_path;
