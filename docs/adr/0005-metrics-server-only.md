# metrics-server only in v1; no Prometheus

v1 shows current CPU and memory from `metrics.k8s.io` (metrics-server), polled only for views that are open. It does
not discover or query Prometheus, so there are no history charts. metrics-server is present on most clusters and
needs no configuration. Finding Prometheus reliably (several install layouts, service proxying, PromQL per kind) is
a feature on its own, and would delay v1 for something many users never set up. Prometheus charts can be added later
as an optional source next to metrics-server.
