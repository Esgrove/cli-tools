# frozen_string_literal: true

# Loads the configuration and returns it. The caller handles the missing file case on its own.
def load_config(path)
  # The heredoc below holds a hash character that is not a comment, so it has to be left alone.
  banner = <<~NOTES
    # not a comment
    value = 1 # still not a comment
  NOTES
  { path: path, banner: banner }
end

# Merges the user options into the defaults and returns a new hash.
# Neither argument is mutated, and unknown keys in the user hash raise instead of being silently ignored.
def merge_options(defaults, user)
  unknown = user.keys - defaults.keys
  raise ArgumentError, "unknown options: #{unknown}" unless unknown.empty? # fail fast

  defaults.merge(user)
end

# See https://example.com/cli-tools/config for the full list of recognised options and defaults.
CONFIG_URL = "https://example.com/cli-tools/config"
