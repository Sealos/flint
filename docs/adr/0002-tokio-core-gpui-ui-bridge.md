# Core runs on its own tokio runtime and talks to the UI only through messages

`kube` needs tokio, and GPUI has its own executor on the main thread. `flint-core` therefore runs a tokio runtime on
background threads. The UI reaches it only through `CoreHandle`: commands go in over channels that work with any
runtime, and coalesced "changed" events come back, at most about 10 per second per view. Views then read immutable
snapshots (`Arc`). Watch events are never forwarded to the UI one by one, and the UI never blocks on core work. This
keeps the main thread responsive during watch storms and keeps `flint-core` testable without a window.

## Consequences

Anything the UI shows must be in a snapshot or arrive as an event. A view cannot simply `await` a kube call. Every
new feature crosses the bridge as an explicit command or event.
