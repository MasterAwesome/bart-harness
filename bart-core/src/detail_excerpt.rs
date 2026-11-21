pub const CODE_BYTES: usize = 16 * 1024;
pub const OUTPUT_BYTES: usize = 64 * 1024;

pub fn head(text: &str, bytes: usize) -> &str {
    &text[..text.floor_char_boundary(text.len().min(bytes))]
}

pub fn tail(text: &str, bytes: usize) -> &str {
    &text[text.ceil_char_boundary(text.len().saturating_sub(bytes))..]
}
