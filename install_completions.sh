#!/bin/bash
set -eo pipefail

# Install shell completions for all CLI tool binaries.

# Import common functions
DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=./common.sh
source "$DIR/common.sh"

USAGE="Usage: $0 [OPTIONS]

Install shell completions for all CLI tool binaries.
Automatically detects the platform and installs completions for the appropriate shells:
  - Windows: bash and powershell
  - macOS:   zsh
  - Linux:   zsh and bash

OPTIONS: All options are optional
    --help
        Display these instructions.

    --verbose
        Display commands being executed.
"

while [ $# -gt 0 ]; do
    case "$1" in
        --help)
            echo "$USAGE"
            exit 1
            ;;
        --verbose)
            set -x
            ;;
        *)
            print_error_and_exit "Unknown option: $1"
            ;;
    esac
    shift
done

cd "$REPO_ROOT"

# Get binary names from Cargo.toml, stripping any .exe suffix
COMPLETION_BINARIES=()
while IFS= read -r name; do
    COMPLETION_BINARIES+=("${name%.exe}")
done <<< "$(get_rust_executable_names)"

if [ ${#COMPLETION_BINARIES[@]} -eq 0 ]; then
    print_error_and_exit "No binaries found in Cargo.toml"
fi

# Determine which shells to install completions for based on platform
case "$BASH_PLATFORM" in
    "windows")
        SHELLS=(bash powershell)
        ;;
    "mac")
        SHELLS=(zsh)
        ;;
    "linux")
        SHELLS=(zsh bash)
        ;;
    *)
        print_error_and_exit "Unknown platform: $BASH_PLATFORM"
        ;;
esac

print_magenta "Installing shell completions for: ${SHELLS[*]}"
echo "Binaries: ${COMPLETION_BINARIES[*]}"
echo ""

FAILED_BINARIES=()
INSTALLED_COUNT=0

for binary in "${COMPLETION_BINARIES[@]}"; do
    if [ -z "$(command -v "$binary")" ]; then
        print_yellow "Skipping $binary: not found in PATH"
        continue
    fi

    for shell in "${SHELLS[@]}"; do
        if "$binary" completion "$shell" -I 2>/dev/null; then
            INSTALLED_COUNT=$((INSTALLED_COUNT + 1))
        else
            print_red "Failed to install $shell completion for $binary"
            FAILED_BINARIES+=("$binary ($shell)")
        fi
    done
done

echo ""

if [ ${#FAILED_BINARIES[@]} -gt 0 ]; then
    print_yellow "Failed completions:"
    for entry in "${FAILED_BINARIES[@]}"; do
        echo "  - $entry"
    done
    echo ""
fi

if [ "$INSTALLED_COUNT" -eq 0 ]; then
    print_error_and_exit "No completions were installed. Are the binaries installed? Run install.sh first."
fi

print_green "Successfully installed $INSTALLED_COUNT completion(s)"

# Print shell-specific sourcing instructions
for shell in "${SHELLS[@]}"; do
    case "$shell" in
        bash)
            echo ""
            print_yellow "Note: For bash completions, ensure your .bashrc sources files from ~/.bash_completion.d/"
            echo "Add to your .bashrc if not already present:"
            echo '  for file in ~/.bash_completion.d/*; do'
            echo '      [ -f "$file" ] && source "$file"'
            echo '  done'
            ;;
        zsh)
            if [ -d "$HOME/.oh-my-zsh/custom/plugins" ]; then
                echo ""
                print_yellow "Note: oh-my-zsh detected. Add the plugins to your .zshrc plugins list:"
                echo "  plugins=(... ${COMPLETION_BINARIES[*]})"
            else
                echo ""
                print_yellow "Note: For zsh completions, ensure your .zshrc includes the completions directory in fpath:"
                echo '  fpath=(~/.zsh/completions $fpath)'
                echo '  autoload -Uz compinit && compinit'
            fi
            ;;
        powershell)
            echo ""
            print_yellow "Note: PowerShell completions are installed to ~/Documents/PowerShell/completions/"
            echo "Ensure your PowerShell profile loads completions from that directory."
            echo 'Add to your $PROFILE if not already present:'
            echo '  Get-ChildItem "$HOME\Documents\PowerShell\completions\*.ps1" | ForEach-Object { . $_ }'
            ;;
    esac
done
