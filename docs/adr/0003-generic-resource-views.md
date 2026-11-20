# Generic, discovery-driven resource views with curated views for core kinds

**Amended by [ADR-0021](./0021-replica-ready-column.md).** Replica workloads show one Flint-computed Ready column in
place of the server's replica count columns, which Wide still shows.

Lists for every kind, CRDs included, come from API discovery. Their rows and columns come from the server's Table
format (`as=Table;g=meta.k8s.io`), the same data `kubectl get` uses. About 10 core kinds (Pod, Deployment,
StatefulSet, DaemonSet, Job, CronJob, Service, Ingress, Node, Namespace) also get hand-built detail views. This gives
correct columns for any CRD without per-kind code, and keeps hand-written UI for the kinds people use most.

## Considered Options

- **Generic only:** least code, but Pods and Nodes would look like raw kubectl output.
- **Hand-built views for built-in kinds (early Lens):** the most code to maintain, and CRDs end up as an
  afterthought.

## Consequences

`kube` has no typed Table support, so lists use raw requests. If watching in Table format proves unreliable, columns
can be computed on the client (built-in definitions plus CRD `additionalPrinterColumns`) without changing this
decision. A curated view is an addition to the generic view: YAML is always available.
