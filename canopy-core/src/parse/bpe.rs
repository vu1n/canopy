//! BPE token estimation with cached encoder.

use std::sync::OnceLock;
use tiktoken_rs::CoreBPE;

/// Cached BPE encoder — initialized once, never panics.
/// `None` means `cl100k_base()` failed; callers fall back to `len/4`.
static BPE_CACHE: OnceLock<Option<CoreBPE>> = OnceLock::new();

fn get_bpe() -> &'static Option<CoreBPE> {
    BPE_CACHE.get_or_init(|| tiktoken_rs::cl100k_base().ok())
}

/// Eagerly initialize the BPE encoder (e.g. before a batch index).
/// Safe to call multiple times; never panics.
pub fn warm_bpe() {
    let _ = get_bpe();
}

/// Estimate token count using tiktoken-rs (cached BPE encoder)
pub fn estimate_tokens(text: &str) -> usize {
    match get_bpe() {
        Some(bpe) => bpe.encode_with_special_tokens(text).len(),
        None => text.len() / 4, // Fallback: rough estimate of 4 chars per token
    }
}
