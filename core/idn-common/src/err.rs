//! `io::Error`s made from a message, by kind: one copy of the `fn bad(m) -> io::Error`
//! every module had written for itself. Each takes anything that prints (a `&str`, a
//! `String`, `format_args!`, another error), so a caller passes what it has.

use std::fmt::Display;
use std::io;

/// An error of no particular kind (`ErrorKind::Other`): something failed.
pub fn other(m: impl Display) -> io::Error {
    io::Error::other(m.to_string())
}

/// Data read that isn't what it should be (`ErrorKind::InvalidData`): a damaged file, a
/// peer's malformed message.
pub fn invalid_data(m: impl Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, m.to_string())
}

/// A caller's argument refused (`ErrorKind::InvalidInput`): a name, a size, a path that
/// can't be used.
pub fn invalid_input(m: impl Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, m.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_messages() {
        let e = other("a");
        assert_eq!(
            (e.kind(), e.to_string()),
            (io::ErrorKind::Other, "a".into())
        );
        let e = invalid_data(format_args!("b {}", 1));
        assert_eq!(
            (e.kind(), e.to_string()),
            (io::ErrorKind::InvalidData, "b 1".into())
        );
        let e = invalid_input(String::from("c"));
        assert_eq!(
            (e.kind(), e.to_string()),
            (io::ErrorKind::InvalidInput, "c".into())
        );
    }
}
