//! Outline rendering: signatures without bodies, indented by nesting.

use crate::symbol::Symbol;

#[derive(Clone, Copy, Debug)]
pub struct OutlineOptions {
    /// Append each symbol's first doc line as `// text`.
    pub docs: bool,
    /// Append `L<start>-<end>` line ranges.
    pub lines: bool,
}

impl Default for OutlineOptions {
    fn default() -> Self {
        OutlineOptions {
            docs: true,
            lines: true,
        }
    }
}

fn depth(symbols: &[Symbol], mut i: usize) -> usize {
    let mut d = 0;
    // `parent` always points backwards, so this terminates; the bound guards corrupt input.
    while let Some(p) = symbols[i].parent {
        let p = p as usize;
        if p >= i || d > 64 {
            break;
        }
        i = p;
        d += 1;
    }
    d
}

/// One line per symbol: `<indent><signature>  L<a>-<b>  // <first doc line>`.
pub fn render_outline(symbols: &[Symbol], opts: &OutlineOptions) -> String {
    let mut out = String::new();
    for (i, s) in symbols.iter().enumerate() {
        for _ in 0..depth(symbols, i) {
            out.push_str("  ");
        }
        out.push_str(&s.signature);
        if opts.lines {
            if s.start_line == s.end_line {
                out.push_str(&format!("  L{}", s.start_line));
            } else {
                out.push_str(&format!("  L{}-{}", s.start_line, s.end_line));
            }
        }
        if opts.docs
            && let Some(first) = s.doc.as_deref().and_then(|d| d.lines().next())
        {
            out.push_str("  // ");
            out.push_str(first);
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbol::{SymbolKind, Visibility};

    fn s(sig: &str, parent: Option<u32>, a: u32, b: u32, doc: Option<&str>) -> Symbol {
        Symbol {
            name: sig.into(),
            kind: SymbolKind::Function,
            visibility: Visibility::Unknown,
            parent,
            start_line: a,
            end_line: b,
            start_byte: 0,
            end_byte: 0,
            signature: sig.into(),
            doc: doc.map(String::from),
        }
    }

    #[test]
    fn indents_by_depth_and_shows_lines_and_docs() {
        let syms = [
            s("class A", None, 1, 9, Some("An A.\nMore.")),
            s("fn f()", Some(0), 2, 2, None),
            s("fn g()", Some(1), 3, 4, None),
            s("fn top()", None, 11, 12, None),
        ];
        let text = render_outline(&syms, &OutlineOptions::default());
        assert_eq!(
            text,
            "class A  L1-9  // An A.\n  fn f()  L2\n    fn g()  L3-4\nfn top()  L11-12\n"
        );
        let bare = render_outline(
            &syms,
            &OutlineOptions {
                docs: false,
                lines: false,
            },
        );
        assert_eq!(bare, "class A\n  fn f()\n    fn g()\nfn top()\n");
    }

    #[test]
    fn corrupt_parent_cycles_cannot_hang() {
        let syms = [s("a", Some(0), 1, 1, None), s("b", Some(1), 2, 2, None)];
        let _ = render_outline(&syms, &OutlineOptions::default());
    }
}
