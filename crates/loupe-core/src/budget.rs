/// Counts tokens for budget packing. Swap in an exact tokenizer by implementing this.
pub trait TokenCounter: Send + Sync {
    fn count(&self, text: &str) -> usize;
}

/// Default approximation: one token per four bytes, rounded up.
#[derive(Clone, Copy, Debug, Default)]
pub struct ApproxTokenCounter;

impl TokenCounter for ApproxTokenCounter {
    fn count(&self, text: &str) -> usize {
        text.len().div_ceil(4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_up() {
        let c = ApproxTokenCounter;
        assert_eq!(c.count(""), 0);
        assert_eq!(c.count("a"), 1);
        assert_eq!(c.count("abcd"), 1);
        assert_eq!(c.count("abcde"), 2);
    }
}
