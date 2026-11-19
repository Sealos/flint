# YAML is validated by server-side dry-run, not a schema engine

The built-in YAML editor highlights syntax but has no schema autocomplete or inline schema errors. Before applying,
Flint sends a server-side dry-run (`dryRun=All`) and shows a diff. The API server is the real validator, admission
webhooks included, so the dry-run catches more than a local schema check would. The common schema tool,
`yaml-language-server`, needs Node, which goes against a native app. Autocomplete can come later from the cluster's
OpenAPI schema with a Rust implementation.
