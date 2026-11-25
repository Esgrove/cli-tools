//! Shell completion script generation, behind the `completion` feature.
//!
//! `generate_shell_completion` prints a completion script for a clap command,
//! or installs it into the completion directory of the user's shell.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Command;
use clap_complete::Shell;

/// Generate a shell completion script for the given shell.
///
/// # Errors
/// Returns an error if:
/// - The shell completion directory cannot be determined or created
/// - The completion file cannot be generated or written
pub fn generate_shell_completion(
    shell: Shell,
    mut command: Command,
    install: bool,
    verbose: bool,
    command_name: &str,
) -> Result<()> {
    if install {
        let out_dir = get_shell_completion_dir(shell, command_name)?;
        let path = clap_complete::generate_to(shell, &mut command, command_name, out_dir)?;
        if verbose {
            println!("Completion file generated to: {}", path.display());
        }
    } else {
        clap_complete::generate(shell, &mut command, command_name, &mut std::io::stdout());
    }
    Ok(())
}

/// Determine the appropriate directory for storing shell completions.
///
/// First checks if the user-specific directory exists,
/// then checks for the global directory.
/// If neither exist, creates and uses the user-specific dir.
fn get_shell_completion_dir(shell: Shell, name: &str) -> Result<PathBuf> {
    let home = dirs::home_dir().context("Failed to get home directory")?;
    shell_completion_dir_in(&home, shell, name)
}

/// Determine the completion directory under the given home directory.
///
/// Taking the home directory as an argument keeps the decision testable.
fn shell_completion_dir_in(home: &Path, shell: Shell, name: &str) -> Result<PathBuf> {
    // Special handling for oh-my-zsh.
    // Create custom "plugin", which will then have to be loaded in .zshrc
    if shell == Shell::Zsh {
        let omz_plugins = home.join(".oh-my-zsh/custom/plugins");
        if omz_plugins.exists() {
            let plugin_dir = omz_plugins.join(name);
            std::fs::create_dir_all(&plugin_dir)?;
            return Ok(plugin_dir);
        }
    }

    let user_dir = match shell {
        Shell::PowerShell => {
            if cfg!(windows) {
                home.join(r"Documents\PowerShell\completions")
            } else {
                home.join(".config/powershell/completions")
            }
        }
        Shell::Bash => home.join(".bash_completion.d"),
        Shell::Elvish => home.join(".elvish"),
        Shell::Fish => home.join(".config/fish/completions"),
        Shell::Zsh => home.join(".zsh/completions"),
        _ => anyhow::bail!("Unsupported shell"),
    };

    if user_dir.exists() {
        return Ok(user_dir);
    }

    let global_dir = match shell {
        Shell::PowerShell => {
            if cfg!(windows) {
                home.join(r"Documents\PowerShell\completions")
            } else {
                home.join(".config/powershell/completions")
            }
        }
        Shell::Bash => PathBuf::from("/etc/bash_completion.d"),
        Shell::Fish => PathBuf::from("/usr/share/fish/completions"),
        Shell::Zsh => PathBuf::from("/usr/share/zsh/site-functions"),
        _ => anyhow::bail!("Unsupported shell"),
    };

    if global_dir.exists() {
        return Ok(global_dir);
    }

    std::fs::create_dir_all(&user_dir)?;
    Ok(user_dir)
}

#[cfg(test)]
mod test_shell_completion_dir {
    use super::*;
    use tempfile::TempDir;

    /// Temporary home directory standing in for the user's own.
    fn home() -> TempDir {
        TempDir::new().expect("temporary directory")
    }

    #[test]
    fn an_oh_my_zsh_install_gets_a_custom_plugin_directory() {
        let home = home();
        std::fs::create_dir_all(home.path().join(".oh-my-zsh/custom/plugins")).expect("plugins directory");

        let directory = shell_completion_dir_in(home.path(), Shell::Zsh, "slb").expect("a directory should be chosen");

        assert_eq!(directory, home.path().join(".oh-my-zsh/custom/plugins/slb"));
        assert!(directory.is_dir(), "the plugin directory should be created");
    }

    #[test]
    fn oh_my_zsh_handling_only_applies_to_zsh() {
        let home = home();
        std::fs::create_dir_all(home.path().join(".oh-my-zsh/custom/plugins")).expect("plugins directory");
        let expected = home.path().join(".config/fish/completions");
        std::fs::create_dir_all(&expected).expect("fish directory");

        let directory = shell_completion_dir_in(home.path(), Shell::Fish, "slb").expect("a directory should be chosen");

        assert_eq!(directory, expected);
    }

    #[test]
    fn an_existing_user_directory_is_used_as_it_is() {
        let home = home();
        let expected = home.path().join(".bash_completion.d");
        std::fs::create_dir_all(&expected).expect("bash directory");

        let directory = shell_completion_dir_in(home.path(), Shell::Bash, "slb").expect("a directory should be chosen");

        assert_eq!(directory, expected);
    }

    #[test]
    fn a_missing_directory_is_created_for_the_user() {
        let home = home();
        let expected = if cfg!(windows) {
            home.path().join(r"Documents\PowerShell\completions")
        } else {
            home.path().join(".config/powershell/completions")
        };

        let directory =
            shell_completion_dir_in(home.path(), Shell::PowerShell, "slb").expect("a directory should be chosen");

        assert_eq!(directory, expected);
        assert!(directory.is_dir(), "the directory should be created");
    }

    #[test]
    fn a_shell_without_a_known_directory_is_rejected() {
        let home = home();

        let error = shell_completion_dir_in(home.path(), Shell::Elvish, "slb")
            .expect_err("elvish has no global directory to fall back to");

        assert!(error.to_string().contains("Unsupported shell"), "{error}");
    }

    #[test]
    fn an_existing_elvish_directory_is_used() {
        let home = home();
        let expected = home.path().join(".elvish");
        std::fs::create_dir_all(&expected).expect("elvish directory");

        let directory =
            shell_completion_dir_in(home.path(), Shell::Elvish, "slb").expect("a directory should be chosen");

        assert_eq!(directory, expected);
    }
}
