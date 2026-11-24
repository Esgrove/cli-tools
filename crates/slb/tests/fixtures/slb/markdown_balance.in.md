# Balance

The backend, the frontend, and the public API all describe archive entry operations from the **Archive Tool's** perspective, not the server's:

- the backend owns the schema
- the frontend owns the labels

Without an explicit account list, `--upload` targets `alpha`, `beta`, `delta`, `gamma`, `kappa`, `lambda`, `sigma`, and `theta`, because the tool runs one account per environment.
`--terraform` requires `--upload`.

## Links and code

See the [configuration reference](https://example.com/cli-tools/config) for the full list of options; the defaults section covers every tool.

The `find_boundaries` function returns candidates ranked by [`Rank`](../src/semantic_line_breaks/rank.rs), and a caller picks the highest ranked one that still fits the width.

> Every fixture pair must round trip: formatting the expected output again must not change it, and that invariant is checked by every fixture test.
