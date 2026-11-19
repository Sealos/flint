# No extension or plugin API in v1

Freelens has extensions, but Flint v1 does not. A public plugin API, whether WASM through `wasmtime` or something
else, would freeze interfaces on top of GPUI while GPUI's own API is still changing. Built-in views are written
behind an internal trait so that a plugin API can wrap them later, once the internal shapes have settled.
