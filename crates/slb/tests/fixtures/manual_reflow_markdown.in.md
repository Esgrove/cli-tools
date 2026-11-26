# Anonymized manual reflow cases

The coordinator stores a continuously updated report so command-line clients, dashboards, and integration tests can inspect progress without correlating several event streams.

The request crosses the worker boundary (where the transport owns retries and timeout handling), then reaches the service adapter which performs the operation and records the result.

- The launcher reads the configuration, prepares the workspace, starts the generic application, and waits for the readiness signal; when readiness fails it records the last diagnostic and shuts down the worker.
- A read-only request looks up the handler in `HandlerRegistry`, moves execution to the main thread, and returns the serialized result without constructing a temporary session.
  The same request path works before a session starts and after one has completed.
- A forced restart cancels an active session, waits for the worker to stop, and submits the restart action; a normal restart refuses to interrupt active work.
  - The cancellation path preserves the original session identifier so clients can explain why the work stopped.
  - The restart path creates a fresh identifier and reports it to the caller.

> Every fixture pair must round trip: formatting the expected output again must not change it, and no fixable violation may remain in the result.

Use the [configuration reference](https://example.com/automation/configuration) to inspect defaults, and run `examplectl session wait --timeout 30` when the service needs more time.

The formatter keeps **strong phrases with several words together**, quoted text such as "a grouped phrase with punctuation, identifiers, and values", and parenthetical notes (including nested details [that remain attached]) whenever those spans fit on one line.
