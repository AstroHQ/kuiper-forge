//! Shell escaping utilities to prevent command injection in remote SSH commands.

/// Escape a string for safe use in a POSIX shell single-quoted context.
///
/// Wraps the value in single quotes and escapes any embedded single quotes
/// using the `'\''` idiom (end quote, escaped literal quote, restart quote).
///
/// # Examples
/// ```
/// # use kuiper_agent_lib::shell::escape_posix;
/// assert_eq!(escape_posix("hello"), "'hello'");
/// assert_eq!(escape_posix("it's"), "'it'\\''s'");
/// assert_eq!(escape_posix("a;b"), "'a;b'");
/// ```
pub fn escape_posix(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Escape a string for safe use in a PowerShell single-quoted context.
///
/// Wraps the value in single quotes and escapes any embedded single quotes
/// by doubling them (`'` → `''`), which is the PowerShell convention.
///
/// # Examples
/// ```
/// # use kuiper_agent_lib::shell::escape_powershell;
/// assert_eq!(escape_powershell("hello"), "'hello'");
/// assert_eq!(escape_powershell("it's"), "'it''s'");
/// ```
pub fn escape_powershell(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_no_special_chars() {
        assert_eq!(escape_posix("hello"), "'hello'");
    }

    #[test]
    fn posix_with_single_quote() {
        assert_eq!(escape_posix("it's"), "'it'\\''s'");
    }

    #[test]
    fn posix_with_shell_metacharacters() {
        assert_eq!(escape_posix("a;b&&c|d"), "'a;b&&c|d'");
    }

    #[test]
    fn posix_with_command_substitution() {
        assert_eq!(escape_posix("$(rm -rf /)"), "'$(rm -rf /)'");
    }

    #[test]
    fn posix_with_backticks() {
        assert_eq!(escape_posix("`whoami`"), "'`whoami`'");
    }

    #[test]
    fn posix_empty_string() {
        assert_eq!(escape_posix(""), "''");
    }

    #[test]
    fn powershell_no_special_chars() {
        assert_eq!(escape_powershell("hello"), "'hello'");
    }

    #[test]
    fn powershell_with_single_quote() {
        assert_eq!(escape_powershell("it's"), "'it''s'");
    }

    #[test]
    fn powershell_with_metacharacters() {
        assert_eq!(escape_powershell("a;b|c"), "'a;b|c'");
    }

    #[test]
    fn powershell_empty_string() {
        assert_eq!(escape_powershell(""), "''");
    }
}
