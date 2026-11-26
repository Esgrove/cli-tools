"""Demonstrate Python docstring detection versus runtime string literals.
Real docstrings are formatted; runtime string values are preserved byte for byte."""


def normal_docstring(items: list[str]) -> list[str]:
    """Normalize the given items and return a fresh list; the input list is never mutated.

    Args:
        items: input entries that may contain
            extra whitespace.

    Returns:
        Cleaned entries in their original order.
    """
    return [item.strip() for item in items]


def escaped_triple_quote_docstring() -> None:
    """
    Show an escaped triple quote \"\"\" inside a docstring body.
    The scanner must not close the docstring early; subsequent sentences still format.
    """
    return None


def opening_line_list_docstring() -> None:
    """- First bullet starts on the opening quote line; it stays on its own item line.
    - Second bullet item; formatted cleanly.
    """
    return None


def standalone_closing_quote_docstring(value: str) -> str:
    """Wrap a short summary of the
    given function value.
    """
    return value


def runtime_fstring_and_bytes(user: str) -> tuple[str, bytes]:
    # Neither f-strings nor byte literals are docstrings in Python; leave both untouched.
    greeting = f"""
    Welcome {user}; this formatted string literal is evaluated at runtime.
    """
    payload = b"""
    Raw protocol header; this bytes literal is runtime data.
    """
    return greeting, payload


def implicit_concatenation_and_continuations() -> tuple[str, str]:
    # Implicit string concatenation and backslash continuations after string hashes are values,
    # never docstrings, so their contents must remain completely verbatim.
    combined = (
        "prefix: "
        """
        Implicitly concatenated body; must not be formatted.
        """
    )
    continued = "https://example.com/docs#section" + \
        """
        Continued after a URL string containing a hash; must not be formatted.
        """
    return combined, continued


def standalone_fstring_statement(name: str) -> None:
    f"""
    Even as a bare expression statement; an f-string is not a docstring.
    """
    b"""
    Even as a bare expression statement; a bytes literal is not a docstring.
    """
    print(name)  # log the active name
