use std::path::PathBuf;
use std::sync::LazyLock;

const PROJECT_NAME: &str = env!("CARGO_PKG_NAME");

pub static CONFIG_PATH: LazyLock<Option<PathBuf>> = LazyLock::new(|| {
    let home_dir = dirs::home_dir()?;

    // Config file path: "$HOME/.config/<PROJECT_NAME>.toml"
    let config_path = home_dir.join(".config").join(format!("{PROJECT_NAME}.toml"));

    if config_path.exists() {
        Some(config_path)
    } else {
        None
    }
});
