use std::process::Command;

/// Checks whether an executable is available through the current PATH.
pub(crate) fn command_exists(command: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {command} >/dev/null 2>&1")])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::command_exists;

    #[test]
    fn finds_shell_builtins_and_standard_commands() {
        assert!(command_exists("sh"));
    }

    #[test]
    fn rejects_missing_commands() {
        assert!(!command_exists("xkeen-sub-command-that-does-not-exist"));
    }
}
