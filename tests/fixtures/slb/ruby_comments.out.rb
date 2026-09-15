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
