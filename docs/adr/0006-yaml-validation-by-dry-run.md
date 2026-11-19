# YAML is validated in three layers: tree-sitter, the cluster's schema, then a server dry-run

The YAML editor validates in three layers.

1. **Syntax (tree-sitter):** `tree-sitter-yaml` parses incrementally on every keystroke, survives errors, and marks
   syntax errors inline. It also maps YAML paths to line and column, so errors from every layer are underlined in
   the right place.
2. **Schema (local):** the edited object is checked against the OpenAPI v3 schema fetched from the cluster
   (`/openapi/v3`) and cached per cluster. This catches unknown fields, wrong types, missing required fields, and
   invalid enum values for every built-in kind and every CRD the cluster serves, without a network call per
   keystroke.
3. **Server dry-run:** before Apply, Flint sends a `PUT` with `dryRun=All` and shows a diff. This remains the final
   check, because only the API server knows about admission webhooks, policies, and immutable fields.

Tree-sitter alone cannot check Kubernetes semantics, and a dry-run alone gives feedback only when the user asks for
it. The schema comes from the cluster itself, so CRDs and version differences are handled without Flint shipping
schemas of its own.

## Considered Options

- **Dry-run only** (this ADR's original decision): simplest, but errors appear only after clicking Review.
- **yaml-language-server:** complete, but needs Node, which goes against a native app.
- **Bundled schemas (kubeconform-style):** they drift from the cluster's actual version and miss its CRDs.

## Consequences

- The validator lives in `flint-core`, with no GPUI dependency. It needs adapters for Kubernetes schema extensions (`x-kubernetes-int-or-string`, `x-kubernetes-preserve-unknown-fields`,
  `x-kubernetes-embedded-resource`) and must resolve `$ref`s across the OpenAPI v3 documents of each group version.
- Schema autocomplete becomes possible from the same data, but it is not in v1.
