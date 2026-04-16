pub fn hamming_wrapper(a: &str, b: &str) -> Option<usize> {
    strsim::hamming(a, b).ok()
}
