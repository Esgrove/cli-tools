"""Module docstring that is wrapped
in the middle of a clause."""

import os  # needed for paths


def parse(path: str) -> dict:
    """Parse the file at the given path and return
    the parsed mapping; missing files raise.

    Args:
        path: the file path that is
            documented here.

    Returns:
        The mapping.
    """
    return {}  # type: ignore


def merge(defaults: dict, overrides: dict) -> dict:
    """Merge two mappings and return a new one; neither argument is mutated, and a shared
    key takes its value from ``overrides`` instead of from ``defaults``.

    See https://example.com/cli-tools/config for the full precedence rules across every
    configuration source, including environment variables and command line flags.

    Args:
        defaults: the base mapping, used for every key that ``overrides`` does not set.
        overrides: the mapping whose values win when the same key appears in both.

    Returns:
        A new mapping holding every key from both arguments.
    """
    result = dict(defaults)
    result.update(overrides)  # overrides always win
    return result


def describe(value: str) -> str:
    """Collect the parts of the value and return them as one line.
    The order of the parts is kept; duplicates are dropped."""
    return value


def label(value: str) -> str:
    """Return the label
    of the given value."""
    return value
