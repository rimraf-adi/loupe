//! Task-level localization eval (docs/v1.md section 8.2) and shared bench helpers.
//!
//! A *task* is a query (a commit subject) plus ground truth (the files that commit changed, which
//! still exist at HEAD). An *arm* turns a query into a ranked list of files. The harness measures
//! recall@k, tokens returned, and latency per arm, so each later milestone can show whether its
//! machinery beats the baselines on the same fixed task set.

pub mod eval;
pub mod mine;
