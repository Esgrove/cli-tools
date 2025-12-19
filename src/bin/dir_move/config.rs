use std::fs;

use itertools::Itertools;
use serde::Deserialize;

use cli_tools::print_error;

use crate::DirMoveArgs;

/// Final config combined from CLI arguments and user config file.
#[derive(Debug)]
pub struct Config {
    pub(crate) auto: bool,
    pub(crate) create: bool,
    pub(crate) debug: bool,
    pub(crate) dryrun: bool,
    pub(crate) include: Vec<String>,
    pub(crate) exclude: Vec<String>,
    pub(crate) min_group_size: usize,
    pub(crate) overwrite: bool,
    pub(crate) prefix_ignores: Vec<String>,
    pub(crate) prefix_overrides: Vec<String>,
    pub(crate) recurse: bool,
    pub(crate) verbose: bool,
    pub(crate) unpack_directory_names: Vec<String>,
}

/// Config from the user config file
#[derive(Debug, Default, Deserialize)]
struct DirMoveConfig {
    #[serde(default)]
    auto: bool,
    #[serde(default)]
    create: bool,
    #[serde(default)]
    debug: bool,
    #[serde(default)]
    dryrun: bool,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    min_group_size: Option<usize>,
    #[serde(default)]
    overwrite: bool,
    #[serde(default)]
    prefix_ignores: Vec<String>,
    #[serde(default)]
    prefix_overrides: Vec<String>,
    #[serde(default)]
    recurse: bool,
    #[serde(default)]
    verbose: bool,
    #[serde(default)]
    unpack_directories: Vec<String>,
}

/// Wrapper needed for parsing the user config file section.
#[derive(Debug, Default, Deserialize)]
struct UserConfig {
    #[serde(default)]
    dirmove: DirMoveConfig,
}

impl DirMoveConfig {
    /// Try to read user config from the file if it exists.
    /// Otherwise, fall back to default config.
    fn get_user_config() -> Self {
        cli_tools::config::CONFIG_PATH
            .as_deref()
            .and_then(|path| {
                fs::read_to_string(path)
                    .map_err(|e| {
                        print_error!("Error reading config file {}: {e}", path.display());
                    })
                    .ok()
            })
            .and_then(|config_string| {
                toml::from_str::<UserConfig>(&config_string)
                    .map_err(|e| {
                        print_error!("Error reading config file: {e}");
                    })
                    .ok()
            })
            .map(|config| config.dirmove)
            .unwrap_or_default()
    }
}

impl Config {
    /// Create config from given command line args and user config file.
    pub fn from_args(args: DirMoveArgs) -> Self {
        let user_config = DirMoveConfig::get_user_config();
        let include: Vec<String> = user_config.include.into_iter().chain(args.include).unique().collect();
        let exclude: Vec<String> = user_config.exclude.into_iter().chain(args.exclude).unique().collect();

        let prefix_ignores: Vec<String> = user_config
            .prefix_ignores
            .into_iter()
            .chain(args.prefix_ignore)
            .unique()
            .collect();

        let prefix_overrides: Vec<String> = user_config
            .prefix_overrides
            .into_iter()
            .chain(args.prefix_override)
            .unique()
            .collect();

        let unpack_directory_names: Vec<String> = user_config
            .unpack_directories
            .into_iter()
            .chain(args.unpack_directory)
            .map(|s| s.to_lowercase())
            .unique()
            .collect();

        Self {
            auto: args.auto || user_config.auto,
            create: args.create || user_config.create,
            debug: args.debug || user_config.debug,
            dryrun: args.print || user_config.dryrun,
            include,
            exclude,
            min_group_size: user_config.min_group_size.unwrap_or(args.group),
            overwrite: args.force || user_config.overwrite,
            prefix_ignores,
            prefix_overrides,
            recurse: args.recurse || user_config.recurse,
            verbose: args.verbose || user_config.verbose,
            unpack_directory_names,
        }
    }
}
