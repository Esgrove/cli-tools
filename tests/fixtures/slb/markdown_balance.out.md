# Balance

The backend, the frontend, and the public API all describe archive entry operations from the **Archive Tool's** perspective,
not the server's:

- the backend owns the schema
- the frontend owns the labels

Without an explicit account list, `--upload` targets `alpha`, `beta`, `delta`, `gamma`, `kappa`, `lambda`, `sigma`, and `theta`,
because the tool runs one account per environment.
`--terraform` requires `--upload`.
