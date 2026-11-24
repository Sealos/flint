# Known limits

Behaviour that is accepted for now. Each entry says what happens and what stays safe.

## Keep watching

- **Up to 32 clusters, with no total memory limit.** Flint refuses a 33rd cluster. Each watched kind is capped at
  50,000 objects per cluster, and a kind over its cap is reported as too large rather than counted in part. There is
  no cap on memory across clusters. At 32 clusters the worst case is about 17.6 million cached entries (32 clusters
  by 11 sources by 50,000 entries: the seven watched kinds, warning events, pending claims and the two usage reads)
  and about 62 MB of usage trends.
- **Warm resource lists add up to 1.35 million cached objects.** At most 3 watched clusters keep their resource
  lists in the background, each with at most 8 kinds plus its CustomResourceDefinition list, at 50,000 objects
  per kind: 3 clusters by 9 kinds by 50,000 objects, on top of the keep-watching figure above.
- **Background sign-in consent resets on restart.** A cluster whose watching depends on a sign-in plugin asks again
  after Flint restarts.
- **A credential-only change on a proxy ends sessions and background sign-in consent.** Saved trust, AI exposure and
  keep-watching for a cluster with a static credential stay.

## Alerts and confirmations

- **Alerts can fire for Pending pods.** A pod that stays Pending for over a minute counts as a problem, like in the
  dashboard.
- **On view still watches the seven problem kinds.** The selected cluster and open set members run the problem feed
  in either watch mode, under the same per-kind cap of 50,000 objects. A member whose sign-in runs a program runs it
  in the background only with keep-watching consent.
- **A confirmation that is open when its row is hidden can still proceed.** Hiding the row (a filter, a search) does
  not close the dialog. The write gate still applies when it proceeds.

## Cluster sets

- **A member whose credential becomes static while connecting shows "sign-in needed" until it is selected.**
- **Opening a set that holds a disconnected cluster reconnects it.**
- **The set drill-down keeps one identifier per counted pod until its filter clears.**
