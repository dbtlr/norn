//! Interactive confirm prompt. Reads a line from the given reader, returns
//! true on 'y'/'Y' (case-insensitive trimmed), false on anything else.
//!
//! Test through the reader; production wires it to std::io::stdin.

use std::io::{BufRead, Write};

pub fn confirm<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    prompt: &str,
) -> std::io::Result<bool> {
    write!(writer, "{prompt}")?;
    writer.flush()?;
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let answer = line.trim().to_lowercase();
    Ok(answer == "y" || answer == "yes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn confirm_returns_true_on_y() {
        let mut reader = Cursor::new(b"y\n".to_vec());
        let mut writer = Vec::new();
        assert!(confirm(&mut reader, &mut writer, "Proceed? [y/N] ").unwrap());
    }

    #[test]
    fn confirm_returns_true_on_uppercase_y() {
        let mut reader = Cursor::new(b"Y\n".to_vec());
        let mut writer = Vec::new();
        assert!(confirm(&mut reader, &mut writer, "Proceed? [y/N] ").unwrap());
    }

    #[test]
    fn confirm_returns_true_on_yes() {
        let mut reader = Cursor::new(b"yes\n".to_vec());
        let mut writer = Vec::new();
        assert!(confirm(&mut reader, &mut writer, "Proceed? [y/N] ").unwrap());
    }

    #[test]
    fn confirm_returns_false_on_n() {
        let mut reader = Cursor::new(b"n\n".to_vec());
        let mut writer = Vec::new();
        assert!(!confirm(&mut reader, &mut writer, "Proceed? [y/N] ").unwrap());
    }

    #[test]
    fn confirm_returns_false_on_empty() {
        let mut reader = Cursor::new(b"\n".to_vec());
        let mut writer = Vec::new();
        assert!(!confirm(&mut reader, &mut writer, "Proceed? [y/N] ").unwrap());
    }

    #[test]
    fn confirm_returns_false_on_garbage() {
        let mut reader = Cursor::new(b"maybe\n".to_vec());
        let mut writer = Vec::new();
        assert!(!confirm(&mut reader, &mut writer, "Proceed? [y/N] ").unwrap());
    }
}
