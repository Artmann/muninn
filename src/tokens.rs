use tiktoken_rs::cl100k_base_singleton;

/// Number of `cl100k_base` tokens in `text`. Used for the recall budget.
pub fn count_tokens(text: &str) -> usize {
    cl100k_base_singleton()
        .encode_with_special_tokens(text)
        .len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_zero() {
        assert_eq!(count_tokens(""), 0);
    }

    #[test]
    fn hello_world_is_two() {
        assert_eq!(count_tokens("hello world"), 2);
    }

    #[test]
    fn multilingual_counts_positive() {
        assert!(count_tokens("Hej världen, hur mår du?") > 2);
    }
}
