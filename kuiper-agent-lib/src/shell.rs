//! Shell escaping utilities to prevent command injection in remote SSH commands.

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};

/// Encode a PowerShell command for use with `-EncodedCommand`.
///
/// This converts the command to UTF-16LE bytes and Base64 encodes them,
/// which is the format PowerShell expects for `-EncodedCommand`.
///
/// This approach completely avoids cmd.exe parsing issues when invoking
/// PowerShell from cmd.exe (e.g., `powershell -EncodedCommand <base64>`),
/// since Base64 contains no special characters that cmd.exe would interpret.
///
/// # Examples
/// ```
/// # use kuiper_agent_lib::shell::encode_powershell_command;
/// let encoded = encode_powershell_command("Write-Host 'Hello'");
/// // Can be used as: powershell -EncodedCommand <encoded>
/// ```
pub fn encode_powershell_command(cmd: &str) -> String {
    let utf16_bytes: Vec<u8> = cmd.encode_utf16()
        .flat_map(|c| c.to_le_bytes())
        .collect();
    BASE64.encode(&utf16_bytes)
}

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

/// Escape a filesystem path for safe use in a POSIX shell, preserving tilde expansion.
///
/// This function handles paths that start with `~` or `~username` specially,
/// keeping the tilde prefix unquoted so the shell can expand it to the home directory.
/// The rest of the path is safely escaped.
///
/// # Security
///
/// The username portion (if present) is validated to contain only characters valid
/// in Unix usernames (`[a-z_][a-z0-9_-]*`). If the username contains invalid characters
/// (which could be shell metacharacters), the entire path is escaped to prevent injection.
///
/// # Examples
/// ```
/// # use kuiper_agent_lib::shell::escape_posix_path;
/// // Tilde expansion is preserved
/// assert_eq!(escape_posix_path("~/actions-runner"), "~/'actions-runner'");
/// assert_eq!(escape_posix_path("~user/runner"), "~user/'runner'");
///
/// // Paths with spaces are safely escaped
/// assert_eq!(escape_posix_path("~/my runner"), "~/'my runner'");
///
/// // Absolute and relative paths are fully escaped
/// assert_eq!(escape_posix_path("/opt/runner"), "'/opt/runner'");
///
/// // Malicious tilde prefixes are escaped entirely
/// assert_eq!(escape_posix_path("~$(evil)/foo"), "'~$(evil)/foo'");
/// ```
pub fn escape_posix_path(s: &str) -> String {
    if !s.starts_with('~') {
        return escape_posix(s);
    }

    // Find where the tilde prefix ends (at / or end of string)
    let slash_pos = s.find('/');
    let tilde_prefix = match slash_pos {
        Some(pos) => &s[..pos],
        None => s,
    };

    // Validate tilde prefix: must be ~ or ~username
    // Unix usernames typically match [a-z_][a-z0-9_-]*
    let username = &tilde_prefix[1..]; // Skip the ~
    let valid_username = username.is_empty()
        || (username
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
            && username
                .chars()
                .skip(1)
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'));

    if !valid_username {
        // Invalid tilde prefix (could contain shell metacharacters), escape everything
        return escape_posix(s);
    }

    match slash_pos {
        Some(pos) => {
            let rest = &s[pos + 1..]; // Part after the /
            if rest.is_empty() {
                // Just "~/" or "~user/"
                format!("{tilde_prefix}/")
            } else {
                // "~/path" or "~user/path" - escape the path portion
                format!("{}/{}", tilde_prefix, escape_posix(rest))
            }
        }
        None => {
            // Just ~ or ~user with no path after
            tilde_prefix.to_string()
        }
    }
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

    #[test]
    fn encode_powershell_simple_command() {
        // "dir" in UTF-16LE is: 'd'=0x0064, 'i'=0x0069, 'r'=0x0072
        // In little-endian bytes: 64 00 69 00 72 00
        // Base64 of [0x64, 0x00, 0x69, 0x00, 0x72, 0x00] = "ZABpAHIA"
        assert_eq!(encode_powershell_command("dir"), "ZABpAHIA");
    }

    #[test]
    fn encode_powershell_with_quotes() {
        // Command with both single and double quotes should encode safely
        let cmd = r#"Write-Host "Hello 'World'""#;
        let encoded = encode_powershell_command(cmd);
        // Verify it's valid base64 and can be decoded back
        let decoded_bytes = BASE64.decode(&encoded).unwrap();
        let decoded: String = decoded_bytes
            .chunks(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .map(|c| char::from_u32(c as u32).unwrap())
            .collect();
        assert_eq!(decoded, cmd);
    }

    #[test]
    fn encode_powershell_with_dangerous_chars() {
        // Characters that would break cmd.exe parsing should be safely encoded
        let cmd = r#"Set-Location 'C:\test"&calc&"'; .\run.cmd"#;
        let encoded = encode_powershell_command(cmd);
        // Base64 only contains [A-Za-z0-9+/=], no cmd.exe metacharacters
        assert!(encoded.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='));
    }

    #[test]
    fn posix_path_tilde_home() {
        // Just ~ should be preserved
        assert_eq!(escape_posix_path("~"), "~");
    }

    #[test]
    fn posix_path_tilde_slash() {
        // ~/ should be preserved
        assert_eq!(escape_posix_path("~/"), "~/");
    }

    #[test]
    fn posix_path_tilde_with_path() {
        // ~/path should preserve ~ and escape the path
        assert_eq!(escape_posix_path("~/actions-runner"), "~/'actions-runner'");
    }

    #[test]
    fn posix_path_tilde_with_spaces() {
        // Spaces in path should be escaped
        assert_eq!(escape_posix_path("~/my runner"), "~/'my runner'");
    }

    #[test]
    fn posix_path_tilde_user() {
        // ~username should be preserved
        assert_eq!(escape_posix_path("~user"), "~user");
        assert_eq!(escape_posix_path("~_admin"), "~_admin");
        assert_eq!(escape_posix_path("~user123"), "~user123");
        assert_eq!(escape_posix_path("~user-name"), "~user-name");
    }

    #[test]
    fn posix_path_tilde_user_with_path() {
        // ~username/path should preserve ~username and escape the path
        assert_eq!(escape_posix_path("~user/actions-runner"), "~user/'actions-runner'");
        assert_eq!(escape_posix_path("~admin/my runner"), "~admin/'my runner'");
    }

    #[test]
    fn posix_path_absolute() {
        // Absolute paths should be fully escaped
        assert_eq!(escape_posix_path("/opt/runner"), "'/opt/runner'");
        assert_eq!(escape_posix_path("/home/user/runner"), "'/home/user/runner'");
    }

    #[test]
    fn posix_path_relative() {
        // Relative paths should be fully escaped
        assert_eq!(escape_posix_path("./runner"), "'./runner'");
        assert_eq!(escape_posix_path("runner"), "'runner'");
    }

    #[test]
    fn posix_path_malicious_tilde() {
        // Malicious tilde prefixes should be escaped entirely
        assert_eq!(escape_posix_path("~$(whoami)/foo"), "'~$(whoami)/foo'");
        assert_eq!(escape_posix_path("~`id`/foo"), "'~`id`/foo'");
        assert_eq!(escape_posix_path("~;rm -rf/foo"), "'~;rm -rf/foo'");
        assert_eq!(escape_posix_path("~user name/foo"), "'~user name/foo'");
    }

    #[test]
    fn posix_path_with_single_quote() {
        // Single quotes in path should be escaped
        assert_eq!(escape_posix_path("~/it's a runner"), "~/'it'\\''s a runner'");
    }

    #[test]
    fn posix_path_nested() {
        // Nested paths should work
        assert_eq!(escape_posix_path("~/foo/bar/baz"), "~/'foo/bar/baz'");
    }
}
