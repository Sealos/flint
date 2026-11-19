# Spike: Table format + watch for generic resource lists

Run against a throwaway kind cluster, Kubernetes server **v1.37.0**, through `kubectl proxy` and curl. Aggregated API
tested: metrics-server (`metrics.k8s.io/v1beta1`). kube-rs checked: `kube` **5.1.0**.

## Recommendation

**Table + watch is viable for every kind, CRDs included.** Use it.

- **Initial list:** `GET` with `Accept: application/json;as=Table;v=v1;g=meta.k8s.io, application/json` and
  `includeObject=Metadata` (the default). Then **watch from the list's `metadata.resourceVersion`** with the same Accept
  header and `allowWatchBookmarks=true`.
- **`includeObject=Metadata`** is the right choice: it gives `namespace`, `uid`, `resourceVersion`, labels and owner
  references per row, which the UI needs for selection, navigation and all-namespace lists. `None` drops the namespace
  entirely (an all-namespaces Table has no Namespace column), so rows are not addressable. `Object` doubles the size for
  data the list does not display.
- **Client must strip `managedFields`** from each row's metadata on arrival. It is ~64% of the metadata bytes and the
  server offers no way to omit it.
- **Do not store the `Age` cell.** It is a server-rendered string ("81s") frozen at send time. Compute age from
  `creationTimestamp` (and for CRDs with an `Age` date column, from the row's metadata too).
- **Watch parsing rules:** only the first event of a watch carries `columnDefinitions`; later events have `null`.
  Treat `BOOKMARK` by event type only (its single row has junk cells such as `"0/0"`).
- **Streaming list (WatchList)** works with Tables on 1.37 and removes the list+watch race, but only use it behind a
  version/probe check with fallback to list+watch (it is a recent feature, absent on older clusters).
- **Fallback rules:**
  1. Response `kind != "Table"` (aggregated servers that ignore Table return a plain `*List` with 200): fall back to
     name/namespace/age columns built from `items[].metadata`.
  2. Watch returns 405 (metrics.k8s.io): poll the list on an interval (no watch, no `limit`, no list resourceVersion).
  3. 406 / non-list subresources (`/log`, discovery docs): not list endpoints, never request Tables there.
  4. Watch `ERROR` event with code 410 `Expired`: relist and rewatch.
- **kube-rs:** doable with the public API (see section 7). No fork or custom HTTP client needed.

## Evidence

Proxy: `kubectl proxy --port=18099`; `T='application/json;as=Table;v=v1;g=meta.k8s.io'`.

### 1. LIST column definitions and row shape

Top-level shape (all kinds): `{kind:"Table", apiVersion:"meta.k8s.io/v1", metadata:{resourceVersion, continue?, remainingItemCount?}, columnDefinitions:[{name,type,format,description,priority}], rows:[{cells:[...], object}]}`.
`priority: 0` columns are the default `kubectl get` set; `priority: 1` are the `-o wide` extras. Descriptions are long
(pods: ~3 KB of definitions per response), so cache columns per (GVR) and drop them from later events.

| Kind | Columns (name:type, `*` = priority 1) |
|---|---|
| pods | Name, Ready, Status, Restarts, Age, IP\*, Node\*, Nominated Node\*, Readiness Gates\* (all `string`) |
| deployments | Name, Ready, Up-to-date, Available, Age, Containers\*, Images\*, Selector\* |
| nodes | Name, Status, Roles, Age, Version, Internal-IP\*, External-IP\*, OS-Image\*, Kernel-Version\*, Container-Runtime\* |
| CRD `widgets.spike.flint.dev` | Name, **Size:integer**, Color\*, **Age:date** (straight from `additionalPrinterColumns`, `priority` preserved) |
| metrics.k8s.io pods (aggregated) | Name (format `name`), cpu (format `quantity`), memory (format `quantity`), Window (format `duration`) |

Cell types follow the column `type`: CRD `Size` cell is a JSON number (`1`), not a string; deployments' Up-to-date is a
number; pods' Restarts is a string (`"0"`; may be `"1 (2m ago)"`).

Pod row (includeObject=Metadata, trimmed):

```json
{"cells":["podinfo-c6497595d-hd4v2","1/1","Running","0","81s","10.244.0.9","flint-dev-control-plane","<none>","<none>"],
 "object":{"kind":"PartialObjectMetadata","apiVersion":"meta.k8s.io/v1",
  "metadata":{"name":"...","generateName":"...","namespace":"demo","uid":"...","resourceVersion":"...","generation":1,
   "creationTimestamp":"...","labels":{},"annotations":{},"ownerReferences":[{}],"managedFields":[{}]}}}
```

CRD row: `{"cells":["w1",1,"red","5s"],"object":{"kind":"PartialObjectMetadata",...}}`.

Metrics row (aggregated): `{"cells":["podinfo-...","4044282n","17896Ki","14.317s"],"object":{"kind":"PartialObjectMetadata","metadata":{"name","namespace","creationTimestamp","labels"}}}`.
Table `metadata` is `{}` for metrics: **no `resourceVersion`**.

**`includeObject`:**

| Value | `rows[].object` |
|---|---|
| `None` | `null`. No name/namespace/uid/resourceVersion except `cells[0]` (the name). All-namespace lists lose the namespace. |
| `Metadata` (default) | `PartialObjectMetadata`: full `metadata` (incl. managedFields), no spec/status. |
| `Object` | The full typed object (`kind: Pod`, with `spec`, `status`). For metrics: `PodMetrics` with `containers`. |

### 2. WATCH with Table

`GET .../widgets?watch=1&resourceVersion=793&allowWatchBookmarks=true&includeObject=Metadata`. Each line is
`{"type":..., "object": <Table>}` where the Table has **exactly one row**. Event types seen: `ADDED`, `MODIFIED`,
`DELETED`, `BOOKMARK`, `ERROR`.

- **First event only** has `columnDefinitions` (full array); every later event has `"columnDefinitions":null`. The same
  held for a watch with no resourceVersion (initial synthetic ADDED events): first event has columns, rest `null`.
- `metadata.resourceVersion` on the Table = that object's resourceVersion (deletion RV for `DELETED`). During the
  initial sync of a no-RV watch RVs are not monotonic (777, 799, 800) so only trust the RV of the **last** event.
- `DELETED` carries the last-known cells and object:

```json
{"type":"DELETED","object":{"kind":"Table","apiVersion":"meta.k8s.io/v1","metadata":{"resourceVersion":"801"},
 "columnDefinitions":null,"rows":[{"cells":["w2",2,"red","13s"],"object":{"kind":"PartialObjectMetadata", "...": "..."}}]}}
```

- `BOOKMARK` is a Table with a **fake row**: empty/null cells, and the RV in both `metadata.resourceVersion` and the
  row object's metadata:

```json
{"type":"BOOKMARK","object":{"kind":"Table","apiVersion":"meta.k8s.io/v1","metadata":{"resourceVersion":"801"},
 "columnDefinitions":null,"rows":[{"cells":["",null,null,null],"object":{"kind":"PartialObjectMetadata",
 "apiVersion":"meta.k8s.io/v1","metadata":{"resourceVersion":"801"}}}]}}
```

  Bookmarks arrived about every 10 s on an idle watch, and one immediately after a DELETE. For pods the fake cells are
  `["", "0/0", "", "0", "<unknown>", ...]`: never render a bookmark as a row.
- With `includeObject=None`, event rows have `"object":null` and the bookmark still carries
  `object.metadata.resourceVersion`. Use `Table.metadata.resourceVersion` for the bookmark RV in all cases.
- Event size at `includeObject=None`: ~344 bytes per line for pods (329 events from a 200-pod scale-down, 270 MODIFIED +
  59 DELETED).

**Expired RV:** HTTP is **200**, and the stream's first (only) line is a plain Status event; no Table:

```json
{"type":"ERROR","object":{"kind":"Status","apiVersion":"v1","metadata":{},"status":"Failure","message":"too old resource version: 1 (773)","reason":"Expired","code":410}}
```

Handle it in the stream parser: relist and rewatch. A watch with an RV from the far future simply idles (no event in 3 s).
A non-watch LIST with a stale RV does not 410 on this server (`resourceVersion=1&resourceVersionMatch=Exact` returned 200
with an empty Table); do not rely on a list-side 410. Continue-token expiry could not be provoked without etcd
compaction; a tampered token returned an empty Table, a malformed one `400 invalid continue token`.

### 3. Streaming lists (`sendInitialEvents=true`, `resourceVersionMatch=NotOlderThan`)

**Works with Table on 1.37.** `GET .../widgets?watch=1&sendInitialEvents=true&resourceVersionMatch=NotOlderThan&allowWatchBookmarks=true`:
one `ADDED` Table per existing object (first one has `columnDefinitions`), then a `BOOKMARK` with annotation
`k8s.io/initial-events-end: "true"`, then normal events. Cluster-wide pods: 13 ADDED + 1 BOOKMARK.

```json
{"type":"BOOKMARK","object":{"kind":"Table","apiVersion":"meta.k8s.io/v1","metadata":{"resourceVersion":"846"},
 "columnDefinitions":null,"rows":[{"cells":["","0/0","","0","<unknown>","<none>","<none>","<none>","<none>"],
 "object":{"kind":"PartialObjectMetadata","apiVersion":"meta.k8s.io/v1",
 "metadata":{"resourceVersion":"846","annotations":{"k8s.io/initial-events-end":"true"}}}}]}}
```

Detect the end marker via `rows[0].object.metadata.annotations["k8s.io/initial-events-end"]`. This annotation lives on
the row object even with `includeObject=None` (the object is non-null for that bookmark). Without `resourceVersionMatch=NotOlderThan`
the server rejects it: Status (`reason: Invalid`) `sendInitialEvents requires setting resourceVersionMatch to NotOlderThan`.
`sendInitialEvents` on a non-watch LIST is rejected (`sendInitialEvents: Forbidden: sendInitialEvents is forbidden for list`).
Metrics-server cannot watch, so streaming lists do not apply to it.

### 4. Who refuses Table?

A full scan of every listable top-level resource in the preferred version of every group (73 resources, built-ins, CRD,
`metrics.k8s.io`): **all 73 returned `kind: Table` with 200**. Nothing refused it, including aggregated
`metrics.k8s.io` nodes and pods (they build their own Table; columns above, no resourceVersion, no pagination, no
watch).

Refusals and oddities found:

| Request | Result |
|---|---|
| `GET /api`, `/apis/apps/v1` (discovery) with Table-only Accept | **406** `NotAcceptable`: "only the following media types are accepted: application/json, application/yaml, application/vnd.kubernetes.protobuf" |
| `GET pods/<name>/log` with Table-only Accept | **406**, same body. |
| `GET /version`, `/openapi/v3` | 200, plain JSON / text; Accept silently ignored. |
| `GET metrics.k8s.io/.../pods?watch=1` | **405** `MethodNotAllowed`: `watch is not supported on resources of kind "pods.metrics.k8s.io"` |
| `GET pods/<n>/eviction` | 405 (no GET). |
| `GET deployments/web/scale`, `pods/<n>/status` (Table) | 200 Table (single-object Tables work). |
| Accept `...;v=v1beta1;g=meta.k8s.io` | 200, `apiVersion: meta.k8s.io/v1beta1` Table (still served). |

Not reproducible here: an aggregated server that ignores the `as=Table` parameter (old or non-Go servers). By the API
contract those return a plain `PodList`-style JSON with 200, so check `kind` of every response and always send
`application/json` as a lower-priority alternative in Accept (Accept `T, application/json` still returned a Table).

### 5. Pagination

Works with Table. `?limit=5&includeObject=Metadata` on pods (13 total): 3 pages, **same `resourceVersion` on every
page** (904), `continue` + `remainingItemCount` (8, 3, then absent) in `metadata`, and **`columnDefinitions` repeated on
every page** (so ignore all but the first). Continue token is base64 JSON, e.g.
`{"v":"meta.k8s.io/v1","rv":904,"start":"/kube-system/coredns-...\u0000"}`.
Use the final page's (identical) RV for the watch. `limit` is **ignored by metrics.k8s.io** (13 rows returned for
`limit=1`, no `continue`).

### 6. Payload size per row (250 pods in one namespace, minimally configured pause pods, JSON, via proxy)

| Format | Total bytes | Avg bytes / row | Notes |
|---|---|---|---|
| Table `includeObject=None` | 71,220 | **120** (+3,086 once for columns) | no identity beyond name |
| Table `includeObject=Metadata` | 1,051,113 | **1,971** | managedFields = 1,253 of that (64%); ownerReferences 171, labels 50 |
| Table `includeObject=Metadata`, gzip | 52,500 | ~210 | compresses 20x (repetitive) |
| `PartialObjectMetadataList` | 926,626 | ~3,700 | no cells, same metadata |
| Table `includeObject=Object` | 2,082,179 | **3,946** | |
| Plain `PodList` JSON | 1,881,548 | **3,799** (per item) | |
| Plain `PodList` protobuf | 575,545 | ~2,300 | |

Rule of thumb for "watch everything": `Metadata` with `managedFields` dropped is about 700 B of metadata + ~120 B of
cells = **~0.8 KB/row** on the wire (before Rust struct overhead; estimate 2x in memory), versus ~4 KB/row for the
full object. 10,000 pods is ~8 MB wire, ~16 MB resident. Real-world pods (more labels/annotations, longer image names)
run larger; real clusters also have larger managedFields. If memory matters in "watch everything" mode, store `cells`
only plus `{uid, namespace, name, resourceVersion, creationTimestamp}` and drop labels/owners until a row is visible.

### 7. kube-rs (kube 5.1.0)

Yes: **`Client` accepts any `http::Request<Vec<u8>>`**, so the Accept header can be set freely.

- Build the request with `kube::api::Request::new(url_path).list(&ListParams)` / `.watch(&WatchParams, resource_version)`
  (docs: <https://docs.rs/kube/latest/kube/api/struct.Request.html>). These return `http::Request<Vec<u8>>` with
  `Accept: application/json` hard-coded; **overwrite it** with `req.headers_mut().insert(http::header::ACCEPT, "application/json;as=Table;v=v1;g=meta.k8s.io, application/json".parse()?)`.
  `ListParams`/`WatchParams` cover `limit`, `continue_token` and `bookmarks` (verify `sendInitialEvents` support in the
  crate version used); `includeObject` has no field, so append it to the request URI query yourself. Source:
  <https://github.com/kube-rs/kube/blob/main/kube-core/src/request.rs>.
- **Lists:** `Client::request::<TableList>(req)` ([docs](https://docs.rs/kube/latest/kube/struct.Client.html#method.request))
  with your own `Deserialize` struct for the Table. Or `request_text` for raw JSON.
- **Watches:** `Client::request_events::<Table>(req)` returns `Stream<Item = Result<WatchEvent<Table>>>`
  (source: <https://github.com/kube-rs/kube/blob/main/kube-client/src/client/mod.rs>). It reads the body line by line and
  parses each as `WatchEvent<T>`. This fits Table events: `ADDED/MODIFIED/DELETED` deserialize into `T = your Table`;
  `BOOKMARK` deserializes into kube's slim `Bookmark` (needs `kind`, `apiVersion`, `metadata.resourceVersion`, optional
  `annotations` - all present on a Table bookmark, so it parses, and the initial-events-end annotation is available as
  `BookmarkMeta.annotations`); `ERROR` becomes `WatchEvent::Error(Status)` (410 Expired). Note that for a bookmark,
  the row/object fields are dropped by this parse; you only need the RV and annotation anyway.
- **Alternative lower level:** `Client::request_stream` returns an `AsyncBufRead` of the raw body if you want to parse
  lines yourself. `Client::send` returns the raw `Response<Body>`.
- Gotcha noted in source: `request_events` sets `Accept-Encoding: identity` (kube comment: Kubernetes >=1.37 gzips watch
  streams one member per event and the decompression layer only decodes the first member). Our proxy-based curl runs did
  not exercise gzip on watches; keep this behaviour and do not add your own `Accept-Encoding: gzip` on watch requests.
- `kube::runtime::watcher` is typed (`Api<K>` with `Resource`) and sends its own Accept, so it cannot be used for Tables;
  write a small custom list+watch loop on top of `request` + `request_events` (resourceVersion tracking, bookmark RV
  updates, relist on 410, backoff).

## Cleanup

Namespace `spike` and CRD `widgets.spike.flint.dev` were deleted; `kubectl proxy` was stopped. All commands used
`KUBECONFIG=.dev/kubeconfig` only.
