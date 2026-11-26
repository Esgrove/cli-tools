# frozen_string_literal: true

# Loads the configuration and returns it; the caller handles the missing file case on its own.
def load_config(path)
  # The heredoc below holds a hash character that is not a comment, so it has to be left alone.
  banner = <<~NOTES
    # not a comment; semicolons stay verbatim
    value = 1 # still not a comment
  NOTES

  # Indented dashed heredocs and multiline strings also protect hash-prefixed data lines.
  footer = <<-END
    # inside dashed heredoc; stays verbatim
    # wrapped lines stay untouched
  END

  multiline = "First line
# inside double-quoted Ruby string; must not change
Last line"

  { path: path, banner: banner, footer: footer, multiline: multiline }
end

# Merges the user options into the defaults and returns a new hash; neither argument is
# mutated, and unknown keys in the user hash raise instead of being silently ignored.
def merge_options(defaults, user)
  unknown = user.keys - defaults.keys
  raise ArgumentError, "unknown options: #{unknown}" unless unknown.empty? # fail fast

  defaults.merge(user)
end

# See https://example.com/cli-tools/config for the full list of recognised options and defaults.
CONFIG_URL = "https://example.com/cli-tools/config"
