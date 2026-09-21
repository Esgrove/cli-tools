//! Anonymized comment reflow cases derived from manual formatter fixes.

namespace example
{
	/// Adds an event marker for every task/session lifecycle event
	/// so remote failures can be correlated with the active automation session.
	class EventHook
	{
	};

	// Render through the low-level encoder rather than the high-level display list
	// (the legacy engine and the current engine expose the same path), so no variant-specific branch is needed.
	void render();

	// The helper returns the visible content bounds (requiring the requested ancestor to remain visible on the chain).
	// Position ratios include margins.
	// Width and height ratios describe only the content extent.
	void locate();

	// This mirrors the shared `DebugOverlay::drawText`
	// and is the rendering path exposed by both engine variants the adapter supports.
	void draw();

	// Out-of-band runtime helpers exist for operations that must take place without going through `session.create`
	// (which serializes sessions and rejects another request while one is active).
	// The helpers below are shared dispatch primitives that every runtime handler uses.
	// Per-operation handlers only choose an action and an argument shape.
	void dispatch();

	// Three dispatch shapes:
	//
	// 1. Read-only query (`runtime.info.*`):
	//    look up the query runner in `HandlerRegistry`, move to the main thread through `runOnMainThread`,
	//    run the query inline, and return the result.
	//
	// 2. Atomic flag update (`runtime.debug.set_overlay`):
	//    build the action, then run its body inline on the main thread.
	//    The action context is a stub and detached actions must not inspect its reporter or parameters.
	//
	// 3. Cancel-then-run action (`runtime.service.restart`):
	//    if a session is active and `force=false`, return a clear error.
	//    With `force=true`, cancel the active session, wait for its worker to stop, and then dispatch as shape 2.
	void shapes();

	// Polling uses wall-clock time because the purpose is to wait for a predicate to become true,
	// while engine-time sleeps advance the simulation by a measured amount.
	void poll();

	/// Every `host.sleep` and `host.waitUntil` call (and every helper that delegates to `bridge.waitFor`)
	/// scales proportionally so task timing stays synchronized with the engine multiplier.
	void wait();
}
