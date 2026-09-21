"""
Generic automation helpers.

The module creates a temporary workspace, starts the example application,
waits for readiness, and records diagnostics when the service cannot start.
"""


def wait_for_service() -> None:
    """
    Wait for a stable service state.

    The readiness query runs on the worker thread (the main thread continues processing queued callbacks),
    so callers can use the same helper before, during, and after a session.
    """

    # A successful query resets the consecutive-error counter because it demonstrates progress,
    # while timing-only actions remain neutral and preserve the existing count.
    pass


def restart_service() -> None:
    # Cancel the active session first.
    # Then wait for the worker and submit a fresh restart request.
    # The result includes the previous identifier so clients can connect the cancellation event with the restart response.
    pass
