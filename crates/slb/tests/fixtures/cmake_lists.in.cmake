# Othello C++ is a small command line game. Akseli Lukkarila maintains it, 2019-2026.
cmake_minimum_required(VERSION 3.31)

project("Othello C++"
    VERSION 3.0.0
    DESCRIPTION "A simple Othello CLI game implementation in C++"
    HOMEPAGE_URL "https://github.com/Esgrove/othellogame"
    LANGUAGES CXX
)

# The C++ standard is pinned below so every host builds the same dialect. Extensions stay off because they hide portability problems until a second compiler is tried.
set(CMAKE_CXX_STANDARD 23)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)
set(CMAKE_CXX_SCAN_FOR_MODULES OFF)

# Cache variables:
#   BUILD_TESTS                   Build the Catch2 test binary under tests/
#   CMAKE_OSX_DEPLOYMENT_TARGET   Oldest macOS the produced binary should run on

if(APPLE)
    # Use libc++ on macOS when using brew LLVM. Otherwise the link can fail if brew clang is mixed with the system libc++.
    if("${CMAKE_CXX_COMPILER}" MATCHES "/opt/homebrew/opt/llvm/bin/clang\\+\\+")
        add_compile_options(-stdlib=libc++)
        add_link_options(
            -stdlib=libc++
            -L/opt/homebrew/opt/llvm/lib/c++
            -L/opt/homebrew/opt/llvm/lib/unwind
            -Wl,-rpath,/opt/homebrew/opt/llvm/lib/c++
            -Wl,-rpath,/opt/homebrew/opt/llvm/lib/unwind
            -lunwind
        )
    elseif("${CMAKE_CXX_COMPILER}" MATCHES "/usr/local/opt/llvm/bin/clang\\+\\+")
        add_compile_options(-stdlib=libc++)
        add_link_options(
            -stdlib=libc++
            -L/usr/local/opt/llvm/lib/c++
            -L/usr/local/opt/llvm/lib/unwind
            -Wl,-rpath,/usr/local/opt/llvm/lib/c++
            -Wl,-rpath,/usr/local/opt/llvm/lib/unwind
            -lunwind
        )
    endif()
    set(CMAKE_INSTALL_RPATH_USE_LINK_PATH ON)
    set(CMAKE_BUILD_WITH_INSTALL_RPATH ON)
    set(CMAKE_OSX_DEPLOYMENT_TARGET "15.0" CACHE STRING "" FORCE)
    # Create a universal binary. This is disabled because OpenSSL is arch specific by default.
    # set(CMAKE_OSX_ARCHITECTURES arm64 x86_64)
endif()

# Export compile commands for clang-tidy. This only works with the Makefile and Ninja generators.
set(CMAKE_EXPORT_COMPILE_COMMANDS ON)

option(BUILD_TESTS "Build the tests" ON) # Catch2 suite under tests/

# Hashes inside quoted and bracket arguments are string data, not comments, so they must be left as they are.
set(HASH_IN_QUOTES "value with a # hash inside")
set(HASH_IN_BRACKETS [[value with a # hash inside]])
set(BRACKET_PROSE [[
# this hash sits inside a bracket argument
message("still not a comment")
]])

# https://cmake.org/cmake/help/latest/module/FetchContent.html
include(FetchContent)

message(STATUS "CMake version: ${CMAKE_VERSION}")
message(STATUS "CMake source dir: ${CMAKE_SOURCE_DIR}")

# Read the short git hash. Configure still succeeds when git is missing or this is an export without a repository.
execute_process(
    COMMAND git rev-parse --short HEAD
    OUTPUT_VARIABLE GIT_COMMIT
    OUTPUT_STRIP_TRAILING_WHITESPACE
    ERROR_QUIET
)
# Read the current branch name. The same missing git case is swallowed so a tarball configure stays quiet.
execute_process(
    COMMAND git branch --show-current
    OUTPUT_VARIABLE GIT_BRANCH
    OUTPUT_STRIP_TRAILING_WHITESPACE
    ERROR_QUIET
)

string(TIMESTAMP BUILD_TIME "%Y-%m-%d_%H%M" UTC)

set(APP_BUILD_NAME "othello_cpp_${CMAKE_PROJECT_VERSION}_${GIT_COMMIT}_${BUILD_TIME}")

message(STATUS "Build name: ${APP_BUILD_NAME}")

if(WIN32)
    # Set the OpenSSL search path for Windows. Scoop, Chocolatey, and the slproweb installer each use a different layout.
    set(SCOOP_OPENSSL_PATH "$ENV{USERPROFILE}/scoop/apps/openssl/current")
    if(EXISTS "${SCOOP_OPENSSL_PATH}")
        set(OPENSSL_ROOT_DIR "${SCOOP_OPENSSL_PATH}")
        set(OPENSSL_INCLUDE_DIR "${SCOOP_OPENSSL_PATH}/include")
        message(STATUS "Found OpenSSL installed via Scoop")
    elseif(EXISTS "C:/Program Files/OpenSSL/include")
        set(OPENSSL_ROOT_DIR "C:/Program Files/OpenSSL")
        set(OPENSSL_INCLUDE_DIR "C:/Program Files/OpenSSL/include")
        # The slproweb Win64OpenSSL installer (used by Chocolatey) places libraries in subdirectories that CMake's FindOpenSSL doesn't search by default.
        list(APPEND CMAKE_LIBRARY_PATH
            "C:/Program Files/OpenSSL/lib/VC/x64/MD"
            "C:/Program Files/OpenSSL/lib/VC/x64/MT"
            "C:/Program Files/OpenSSL/lib/VC"
            "C:/Program Files/OpenSSL/lib/MinGW/x64"
            "C:/Program Files/OpenSSL/lib"
        )
        message(STATUS "Found OpenSSL at: ${OPENSSL_ROOT_DIR}")
    else()
        message(STATUS "OpenSSL not found in common Windows locations")
    endif()
endif()

# OpenSSL is required at link time. The Crypto target is enough because the game only hashes the board, it never opens a TLS socket.
find_package(OpenSSL REQUIRED)
if(OPENSSL_FOUND)
    message(STATUS "Found OpenSSL: ${OPENSSL_INCLUDE_DIR}")
    message(STATUS "Found OpenSSL version: ${OPENSSL_VERSION}")
else()
    message(FATAL_ERROR "OpenSSL not found")
endif()

# ccache
# https://ccache.dev/
find_program(CCACHE_EXECUTABLE ccache)
if(CCACHE_EXECUTABLE)
    message(STATUS "ccache found: ${CCACHE_EXECUTABLE}")
    # Print the first line of ccache --version so the log shows which binary configure picked.
    execute_process(
        COMMAND ${CCACHE_EXECUTABLE} --version
        OUTPUT_VARIABLE CCACHE_VERSION_INFO
        OUTPUT_STRIP_TRAILING_WHITESPACE
    )
    string(REGEX MATCH "^[^\n]*" CCACHE_VERSION "${CCACHE_VERSION_INFO}")
    message(STATUS "${CCACHE_VERSION}")
    set(CMAKE_C_COMPILER_LAUNCHER ${CCACHE_EXECUTABLE})
    set(CMAKE_CXX_COMPILER_LAUNCHER ${CCACHE_EXECUTABLE})
endif(CCACHE_EXECUTABLE)

# fmt is an open source formatting library. It is a fast and safe alternative to C stdio and C++ iostreams.
# https://github.com/fmtlib/fmt
# This can be used if fmt is installed locally (brew, vcpkg, apt...)
# find_package(fmt 11 REQUIRED)
message(STATUS "Fetching fmt library")
FetchContent_Declare(fmt
    GIT_REPOSITORY https://github.com/fmtlib/fmt
    GIT_TAG 12.1.0
    SYSTEM
)
FetchContent_MakeAvailable(fmt)

# CLI argument parsing library. A vendored copy keeps the flags stable across packagers.
# https://github.com/jarro2783/cxxopts
message(STATUS "Fetching cxxopts library")
FetchContent_Declare(cxxopts
    GIT_REPOSITORY https://github.com/jarro2783/cxxopts
    GIT_TAG v3.3.1
    SYSTEM
)
FetchContent_MakeAvailable(cxxopts)

# Stamp the binary with the project name, version, and the git identity from configure time.
set(VERSION_INFO_DEFINITIONS
    COMPILE_TIME_APP_NAME="${CMAKE_PROJECT_NAME}"
    COMPILE_TIME_GIT_BRANCH="${GIT_BRANCH}"
    COMPILE_TIME_GIT_COMMIT="${GIT_COMMIT}"
    COMPILE_TIME_BUILD_TIME="${BUILD_TIME}"
    COMPILE_TIME_VERSION_NUMBER="${CMAKE_PROJECT_VERSION}"
    COMPILE_TIME_VERSION_STRING="${CMAKE_PROJECT_NAME} ${CMAKE_PROJECT_VERSION} ${BUILD_TIME} ${GIT_BRANCH} ${GIT_COMMIT}"
)

set_property(GLOBAL PROPERTY GLOBAL_COMPILE_DEFINITIONS "${VERSION_INFO_DEFINITIONS}")

add_executable(othello_cpp)

# Keep this list sorted so a review can see a newly added source without scanning the whole target definition by hand.
target_sources(othello_cpp PRIVATE
    src/board.cpp
    src/main.cpp
    src/models.cpp
    src/othello.cpp
    src/player.cpp
    src/utils.cpp
)

target_include_directories(othello_cpp PRIVATE src)

target_compile_definitions(othello_cpp PRIVATE
    ${VERSION_INFO_DEFINITIONS}
    $<$<CONFIG:Debug>:OTHELLO_DEBUG=1> # debug builds only
)

target_link_libraries(othello_cpp
    cxxopts
    fmt::fmt
    OpenSSL::Crypto
)

# Enable LTO for release builds. The check below is required because some toolchains report IPO as unsupported.
include(CheckIPOSupported)
check_ipo_supported(RESULT result OUTPUT output)
if(result)
    message(STATUS "Using LTO")
    set_target_properties(othello_cpp PROPERTIES INTERPROCEDURAL_OPTIMIZATION_RELEASE TRUE)
else()
    message(STATUS "IPO is not supported: ${output}")
endif()

if(MSVC)
    # https://learn.microsoft.com/en-us/cpp/build/reference/permissive-standards-conformance?view=msvc-170
    target_compile_options(othello_cpp PRIVATE
        /W4 /WX /permissive-
    )
else()
    target_compile_options(othello_cpp PRIVATE
        -Wall -Wextra -Werror -pedantic
    )
    # Skip native CPU targeting on CI: cached build artifacts are shared between runners with different CPUs, so natively-targeted code can crash with SIGILL on another runner.
    if(CMAKE_BUILD_TYPE STREQUAL "Release" AND NOT DEFINED ENV{CI})
        target_compile_options(othello_cpp PRIVATE
            -march=native -mtune=native
        )
    endif()
endif()

if(BUILD_TESTS)
    enable_testing()
    add_subdirectory(tests)
endif()

get_target_property(COMPILE_OPTIONS othello_cpp COMPILE_OPTIONS)
message(STATUS "CMAKE_BUILD_TYPE: ${CMAKE_BUILD_TYPE}")
message(STATUS "CMAKE_CXX_COMPILER: ${CMAKE_CXX_COMPILER}")
message(STATUS "CMAKE_CXX_COMPILER_LAUNCHER: ${CMAKE_CXX_COMPILER_LAUNCHER}")
message(STATUS "COMPILE_OPTIONS: ${COMPILE_OPTIONS}")
