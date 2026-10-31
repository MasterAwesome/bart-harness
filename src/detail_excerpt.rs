pub(crate) const CODE_BYTES: usize = 16 * 1024;
pub(crate) const OUTPUT_BYTES: usize = 64 * 1024;

pub(crate) fn head(text: &str, bytes: usize) -> &str {
    &text[..text.floor_char_boundary(text.len().min(bytes))]
}

pub(crate) fn tail(text: &str, bytes: usize) -> &str {
    &text[text.ceil_char_boundary(text.len().saturating_sub(bytes))..]
}

pub(crate) fn code_notice(complete: bool) -> &'static str {
    if complete {
        "[code excerpt; c copies the complete recorded code]"
    } else {
        "[code excerpt; c copies only the retained excerpt]"
    }
}
