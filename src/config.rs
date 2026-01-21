use std::path::PathBuf;
use std::sync::LazyLock;

const PROJECT_NAME: &str = env!("CARGO_PKG_NAME");

/// Path to the user config file: `$HOME/.config/cli-tools.toml`
///
/// Returns `None` if the home directory cannot be determined.
pub static CONFIG_PATH: LazyLock<Option<PathBuf>> = LazyLock::new(|| {
    let home_dir = dirs::home_dir()?;
    Some(home_dir.join(".config").join(format!("{PROJECT_NAME}.toml")))
});
