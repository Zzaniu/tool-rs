mod tools;

pub use tools::*;

pub fn error_caused_str(mut err: &(dyn std::error::Error + 'static)) -> String {
    use std::fmt::Write;
    let mut msg = format!("{err}");
    while let Some(source) = err.source() {
        let _ = write!(msg, "\n\nCaused by: {}", source);
        err = source;
    }
    msg
}
