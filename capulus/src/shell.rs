pub fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_owned();
    }
    if value.bytes().all(is_safe_atom_byte) {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub fn bash_command(shell_body: &str) -> String {
    format!("bash -lc {}", shell_quote(shell_body))
}

fn is_safe_atom_byte(byte: u8) -> bool {
    matches!(
        byte,
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'.' | b'_' | b'-' | b':'
    )
}

#[cfg(test)]
mod tests {
    use super::{bash_command, shell_quote};

    #[test]
    fn shell_quote_quotes_empty_string() {
        assert_eq!("''", shell_quote(""));
    }

    #[test]
    fn shell_quote_leaves_safe_atoms_unquoted() {
        assert_eq!("/tmp/demo-path", shell_quote("/tmp/demo-path"));
        assert_eq!("https://example.com", shell_quote("https://example.com"));
    }

    #[test]
    fn shell_quote_escapes_apostrophes() {
        assert_eq!("'abc'\"'\"'def'", shell_quote("abc'def"));
    }

    #[test]
    fn bash_command_wraps_shell_body() {
        assert_eq!("bash -lc 'echo hi'", bash_command("echo hi"));
    }
}
