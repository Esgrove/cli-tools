# Configures the C++ sources that the packager installs.
# The cache variables below pin the standard and the search paths used to find the runtime libraries on each host.

cmake_minimum_required(VERSION 3.20)
project(cli_tools LANGUAGES CXX)

# Keep this list sorted so a review can see a newly added source without scanning the whole target definition by hand.

add_library(cli_tools_core STATIC
  src/lib.cpp
)

# See https://example.com/cli-tools/cmake for the full list of recognised cache variables and defaults.
include(GNUInstallDirs)
