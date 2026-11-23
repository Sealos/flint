# Flint

Flint is a fast, native Kubernetes app for your Mac that lets Claude Code and other AI agents read the clusters you
choose, and nothing else.

Its `flint mcp` command is a read-only [MCP](https://modelcontextprotocol.io/) server. The desktop app, in the space of
Lens and Freelens, is where you choose which clusters agents may read. Flint is written in Rust with
[GPUI](https://www.gpui.rs/), so it starts fast, stays light on memory, and keeps up with large clusters. It runs on
macOS and, best effort, Linux.

```sh
claude mcp add flint -- /usr/local/bin/flint mcp     # after installing Flint and exposing a cluster
```

See [Add it to an agent](#add-it-to-an-agent) for the steps and other MCP clients.

## MCP server for AI agents

`flint mcp` serves the Model Context Protocol over stdio. What an agent gets:

- **Only the clusters you choose.** Nothing is exposed by default. You opt in per cluster with **Expose to AI** (the
  robot button next to a cluster in the sidebar, which asks you to confirm the context and its server; exposed
  clusters show an "AI" badge). An exposure is tied to the context's API server URL, so a context that is re-pointed,
  or has the same name in another `KUBECONFIG`, is not exposed until you expose it again. The list is read again on
  every call, so a change in the app applies to the next call without restarting the agent. A call naming any other
  cluster fails as if it did not exist.
- **Only the namespaces and kinds you choose.** The Expose to AI dialog (and Edit AI Scope, from a cluster's agent
  activity or the palette) can limit a cluster to some namespaces and kinds. With namespaces chosen, cluster-wide kinds such as nodes
  are hidden unless you check them. Lists across all namespaces, events and pod logs stay inside the scope, and a
  request outside it gets one "outside the scope" error, the same whether what it named exists or not, so an agent
  cannot probe what else exists.
  Text the agent may read can still name things outside the scope (the node a pod runs on), and an older `flint mcp`
  binary ignores scopes, so point agents at the Flint you configure.
- **Read-only.** The tools list clusters, resource kinds, objects, events, and pod logs, and nothing else: no tool
  creates, changes, or deletes anything, and there is no exec or port-forward.
- **Credentials redacted.** Secret values are always redacted. In pod logs, event messages, other message fields
  (status, conditions, container termination messages, and list columns), annotations and container `args` and
  `command`, values that look like credentials are replaced with a marker such as `[REDACTED:password]`, and each
  result says how many were replaced: `Authorization` and bearer tokens, `Cookie` and `Set-Cookie` headers, passwords
  in URLs and MySQL DSNs, registry `auth` values in docker config JSON, AWS, GCP, GitHub, GitLab, Slack, Stripe, OpenAI
  and Anthropic keys and tokens, Slack webhooks, Azure SAS signatures, JWTs, private key blocks, and the values of
  `password=`, `secret:`, `token=` and `api_key=` style pairs and flags. Flint's own log viewer and events are
  unchanged: they show text as written.
- **Bounded.** Lists, events and logs have default and maximum sizes, and when output is cut short the result says
  so. At most 16 calls run at once (more wait, and past 64 they are refused), each for at most 60 seconds, and an
  agent that stops reading replies stops getting new calls run.
- **Best-effort activity log.** Flint records call metadata in its config directory, including when the call ran,
  the cluster and tool, what it named (kind, namespace, object, container), how much came back, and whether it was
  cut short or denied. A writer that cannot acquire the log lock within one second skips the record. The log stores
  only metadata. Click a cluster's "AI" badge or run Show Agent Activity… from the palette to see its calls as they
  are recorded.

Redaction is pattern matching, so a credential in an unusual format can still get through. Everything else,
including ConfigMaps and `env` values in pod specs, is sent as the cluster returns it. Only expose clusters whose
contents, logs and events you are willing to send to your AI provider. The full rules are in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#mcp-server).

### Add it to an agent

[Install Flint](#install-from-a-release), link its binary to `/usr/local/bin/flint` (the DMG install shows how, the
command-line archive installs it there), expose a cluster in the app, then register the server with Claude Code:

```sh
claude mcp add flint -- /usr/local/bin/flint mcp
```

The path stays the same when you update Flint or switch between the DMG and the archive. After
`cargo build --release`, use `"$PWD/target/release/flint"`.

On Linux, register the AppImage the same way; it passes `mcp` through to `flint`:

```sh
claude mcp add flint -- "$HOME/.local/bin/flint" mcp
```

Without FUSE, give every `flint mcp` launch its own `TMPDIR` (see [Linux](#linux-appimage)), so it cannot clash with
the app or another agent's server:

```sh
claude mcp add flint -- sh -c 'd=$(mktemp -d) || exit 1; TMPDIR=$d APPIMAGE_EXTRACT_AND_RUN=1 "$HOME/.local/bin/flint" mcp; s=$?; rm -rf "$d"; exit $s'
```

Other MCP clients (Claude Desktop, Cursor) take the same command: `/path/to/flint mcp`.

The app and an agent can read different kubeconfig files: an app opened from Finder ignores a `KUBECONFIG` exported in
your shell, and an agent started from a terminal inherits it. If a cluster you exposed is in none of the files the
server read, `list_clusters` says so and lists those files. Add the missing file to `kubeconfig_paths` in
`settings.toml`, which the app and the server both read.

Where the app cannot run, for example on a Linux server or in a devcontainer, install the
[static `flint-mcp`](#linux-mcp-server-only-flint-mcp) and register it the same way, without `mcp`:

```sh
claude mcp add flint -- "$HOME/.local/bin/flint-mcp"
```

### Expose a cluster without the app

The Expose to AI dialog writes one `ai_exposed_clusters` entry to `settings.toml` per cluster. Without the app, add
the entry yourself, in the settings file of the machine the server runs on: `$FLINT_CONFIG_DIR/settings.toml` if
that is set, else `$XDG_CONFIG_HOME/flint/settings.toml` on Linux (default `~/.config/flint/settings.toml`). An entry pins the context name to the API server URL it points to now; print
that URL with:

```sh
kubectl config view --minify --context staging -o jsonpath='{.clusters[0].cluster.server}'
```

```toml
[[ai_exposed_clusters]]
context = "staging"
server = "https://staging.example.com:6443"
# namespaces = ["shop"]                 # optional: only these namespaces (missing or empty: all)
# kinds = ["pods", "deployments.apps"]  # optional: only these kinds (missing or empty: all)
```

Kinds are written as for `kubectl get`: plural, singular, Kind or short name (`pods`, `pod`, `Pod`, `po`), with
`.group` when needed. A name that matches no kind, or several, grants nothing, and so does a name without a group
while part of API discovery is failing. `list_clusters` reports such names as
`unmatched_kinds`, and namespaces that do not exist as `unmatched_namespaces`. The server logs them once to stderr.
When it could not check everything, it says so in `check_incomplete`, with a count in `unchecked_namespaces`.

An empty list (`namespaces = []`) means no limit, not no access: to keep agents away from a cluster, remove its entry.
If the file already has `ai_exposed_clusters` as an inline array (`ai_exposed_clusters = [...]`), add the entry to
that array instead; TOML doesn't allow both forms for one key, and an invalid file turns every exposure off.

- **Pinned.** Both must match: if the context is later re-pointed to another server, or another kubeconfig file has a
  context of the same name at another server, it is hidden from agents until you update `server`. A trailing slash
  does not matter.
- **Checked.** A misspelt key (`namespace` for `namespaces`), or the same context and server listed twice, makes the
  file invalid. Then nothing is exposed: every call fails and says that `settings.toml` is invalid, with the unknown
  field and its line and column but no value from the file. The server logs the same reason on stderr when it starts.
- **Read again on every call**, so an edit applies to the agent's next call without a restart.
- **Read-only either way.** The entry decides what the server may read, never what it may do: it has no tool that
  creates, changes or deletes anything. It reads with the kubeconfig's credentials, so a user with read-only RBAC
  limits it further.

### Compared with other Kubernetes MCP servers

As their documentation describes them in November 2026:

| | Flint (`flint mcp`) | [Lens MCP Server](https://docs.lenshq.io/k8slens/mcp-server/) | [kubernetes-mcp-server](https://github.com/containers/kubernetes-mcp-server) |
|---|---|---|---|
| Which clusters | Only those you expose, one by one, each tied to its API server URL, optionally limited to some namespaces and kinds | Every cluster Lens knows | The contexts in its kubeconfig |
| Writes | None: no tools for them | Its `kubectl` tool refuses writes; editors and Helm dialogs it fills in apply changes after you confirm; some tools change Lens itself (port forwards, settings) | Writes allowed, including delete and pod exec, unless its `read_only` or `disable_destructive` setting is on |
| Credential redaction in results | Secret values, and credentials in logs, events, messages and container arguments | Not described | Not for tool results (only its own log notifications); Secrets can be denied by configuration |

## Status

Flint is in early development.

**Works today:**

- Your kubeconfig contexts in a sidebar; Flint connects to the one you select and checks its health.
- Every kind the cluster serves, CRDs included, from the cluster's own discovery: live lists with the `kubectl get`
  columns, a namespace and text filter, and a detail view with metadata, conditions, events and YAML.
- A watch mode for the active cluster: watch all kinds or only open views, with a default and per-cluster overrides.
- YAML editing with a server dry-run and diff, plus delete, scale, rollout restart, cordon and drain, all behind the
  cluster protection.
- A bottom dock with pod and workload logs (follow, search, timestamps, previous logs, save to a file) and shells into
  containers. Open them from a row's context menu, the detail pane or the keyboard.
- Log filters: Matching Lines Only (M) keeps lines that match your search, and Errors Only (E) keeps warnings and
  errors. Use either filter or both, with adjustable context before and after each match.
- Protected clusters: every cluster not on your machine is read-only in Flint until you unlock it for the session
  or trust it (the lock button in the sidebar, or the palette). That covers shells, edits and resource actions.
- GitOps markers: list rows with Argo CD or Flux tracking metadata show an `Argo` or `Flux` marker, and the detail
  header names the manager. Helm alone does not count; a Flux HelmRelease is recognised by Flux's tracking metadata.
  Outside Secret lists, which show no markers, typing `argo`, `flux` or `managed` in the list filter includes those
  rows. The filter is a plain text search, so it also matches any other row with that text, such as an unmanaged
  `argo-server` or a `managed-by=Helm` label.
- Actions your RBAC clearly forbids (edit, delete, scale, restart, cordon, drain, logs, shell) are disabled and name
  the missing permission. The API server still decides every request.
- Cluster colour tags: six colours, set from a cluster's row menu or the palette, shown as a dot in the sidebar and a
  strip over the detail pane. A tag belongs to a context at one server and changes nothing about access.
- A command palette (⇧⌘P) that runs any action by name and shows its shortcut (see [Keys](#keys)).
- A resource switcher (⌘P) that ranks matches for the active cluster's kinds and objects already in memory. Open a
  kind's list or jump straight to an object's details.
- Go Back (⌘[) returns to the previous kind and object on the active cluster and restores the list's text filter.
  Tab and Shift-Tab cycle through panes outside text fields and shells. Enter opens the selected list row.
- Light and dark themes that follow the system (View > Appearance, or the `appearance` setting, overrides it).
- A read-only MCP server, `flint mcp`, for AI agents, limited to the clusters you expose.
- Builds for macOS (Apple Silicon and Intel) and, best effort, Linux (x86_64 and arm64). From 0.5.0, the 24
  uploaded release files have [build provenance](#verify-a-download); GitHub's source archives do not.

**Coming next:**

- **v0.6, see and watch many clusters:** cluster sets, combined views and watching across clusters.
- **v0.7, know what's wrong:** alerting and health signals across clusters.
- **v0.8, change many clusters:** guarded fleet actions with explicit targets, a dry run per cluster, one cluster at a
  time, and a stop at the first failure.
- **1.0, first public release:** a signed and notarized app with a Homebrew cask, port-forwarding, and read-only Helm
  releases.
- **Later:** curated views for core kinds, [regex search](https://github.com/Sealos/flint/issues/309) and
  [log time ranges](https://github.com/Sealos/flint/issues/265).

## Install from a release

Download from the [latest release](https://github.com/Sealos/flint/releases/latest), or with `gh release download`,
which takes the latest release when given no tag. Run the commands below in an empty directory, so each pattern
matches only the files you just downloaded.

### macOS app (DMG)

Releases after v0.2.0 have `Flint.app` in a disk image, `flint-vX.Y.Z-macos-universal.dmg`, for Apple Silicon and
Intel Macs on macOS 13 or later. v0.2.0 itself has the [command-line archive](#macos-command-line-archive) and the
AppImages only.

1. Download the DMG from the latest release, or with `gh`, and check it:

   ```sh
   gh release download -R Sealos/flint --pattern 'flint-*-macos-universal.dmg*'
   shasum -a 256 -c flint-*-macos-universal.dmg.sha256
   ```

   If `gh` finds no matching asset, the latest release is still v0.2.0: use the
   [command-line archive](#macos-command-line-archive) instead.

2. Open the DMG and drag **Flint** onto **Applications**, then eject the DMG.
3. Open Flint from Applications, Launchpad or Spotlight.

**First launch.** Until the first signed release the app is **not signed with a Developer ID or notarized** (it
carries an ad-hoc signature only), so macOS refuses to open a copy downloaded with a browser the first time. Allow it
once per downloaded version, either way:

- open Flint once, dismiss the warning (**Done**, not Move to Trash), then open **System Settings > Privacy &
  Security**, scroll to the message about Flint, click **Open Anyway**, and confirm; or
- remove the quarantine flag in a terminal: `xattr -dr com.apple.quarantine /Applications/Flint.app`.

On macOS 15 and later, Control-click > Open no longer gets past this check. Until you allow it, macOS also stops
`Flint.app/Contents/MacOS/flint` run from a terminal, `flint mcp` included. A DMG downloaded with `gh` or `curl` is not
quarantined and opens directly.

**Environment.** Opened from Finder, the Dock or Spotlight, Flint gets no terminal environment. It runs your login
shell once at startup to read its `PATH`, so exec credential plugins (`aws`, `gke-gcloud-auth-plugin`, `kubelogin`)
and `helm` are found as in a terminal. If the shell takes longer than 3 seconds or fails, Flint uses the `PATH` it was
started with plus `/opt/homebrew/bin` and `/usr/local/bin`. Set `path` in `settings.toml` (below) to skip the shell
and use your own. Only `PATH` is read: a `KUBECONFIG` exported in your shell is not, so Flint reads `~/.kube/config`
unless you add files with `kubeconfig_paths`.

**Logs.** Flint writes diagnostics to `~/Library/Logs/Flint/flint.log`, including when opened from Finder or the
Dock. Use **Go > Go to Folder** in Finder to open `~/Library/Logs/Flint/`. Before attaching a log to a bug report,
remove cluster names, server URLs and other private details. See [Build and run](#build-and-run) for log levels and rotation.

**The command-line binary.** The app's executable is also the `flint` command, for `flint mcp` (see
[MCP server for AI agents](#mcp-server-for-ai-agents)) and `flint --version`. `flint --help` lists its commands,
options and the environment variables it reads. Until the Homebrew cask puts it on your
`PATH`, link it to `/usr/local/bin/flint`, one stable path for your shell and your MCP client configs. `-f` replaces a
`flint` installed earlier from the command-line archive below:

```sh
sudo mkdir -p /usr/local/bin
sudo ln -sf /Applications/Flint.app/Contents/MacOS/flint /usr/local/bin/flint
flint --version
```

### macOS command-line archive

Each release also has the same universal binary without the app bundle, `flint-vX.Y.Z-macos-universal.tar.gz`. Run it
from a terminal and it opens the Flint window:

```sh
gh release download -R Sealos/flint --pattern 'flint-*-macos-universal.tar.gz*'
shasum -a 256 -c flint-*-macos-universal.tar.gz.sha256
tar -xzf flint-*-macos-universal.tar.gz
sudo install -m 755 flint/flint /usr/local/bin/flint
flint --version
flint                                   # opens the window
```

Downloaded with `gh` or `curl`, it runs as is. Downloaded with a browser, macOS blocks the first run: remove the
quarantine flag with `xattr -d com.apple.quarantine /usr/local/bin/flint`, or open **System Settings > Privacy &
Security** and choose **Open Anyway**.

### Linux (AppImage)

Each release also has an AppImage for x86_64 and aarch64 (arm64). Linux support is best effort: CI builds and
tests it, but it is not tested on every desktop and distribution ([ADR-0016](docs/adr/0016-linux-appimage.md)). It
needs glibc 2.39 or newer (Ubuntu 24.04, Fedora 40, Debian 13 or later). The AppImage is the Linux install; deb and
rpm packages are planned after 1.0.

```sh
gh release download -R Sealos/flint --pattern 'flint-*-linux-x86_64.AppImage*'   # or linux-aarch64
sha256sum -c flint-*-linux-x86_64.AppImage.sha256
install -D -m 755 flint-*-linux-x86_64.AppImage ~/.local/bin/flint
flint --version
flint                                   # opens the window
```

**Host libraries.** The AppImage bundles what a desktop may lack, but takes glibc, fontconfig and libxcb from the
host; without them it does not start at all, not even `--version` or `mcp`. Every desktop has them. On a minimal or
headless system install them (on Debian and Ubuntu: `libfontconfig1 libxcb1`, listed in
[`packaging/linux/ubuntu-runtime-packages.txt`](packaging/linux/ubuntu-runtime-packages.txt); CI runs the AppImage in
a clean `ubuntu:24.04` with only those). The window also needs an X11 or Wayland session and a GPU driver with
Vulkan or OpenGL ES (Mesa on most systems). To run only the MCP server on a headless system, an older distribution or
in a container, use the [static `flint-mcp`](#linux-mcp-server-only-flint-mcp) instead: it needs none of these.

**FUSE.** AppImages mount themselves with FUSE (the `fusermount` or `fusermount3` command). Without it, for example
in a container, set `APPIMAGE_EXTRACT_AND_RUN=1` to extract to `$TMPDIR` and run from there instead. Give each such
launch its own `TMPDIR`: two launches of the same AppImage share one extraction directory, and the first to exit
deletes it under the other.

```sh
TMPDIR=$(mktemp -d) APPIMAGE_EXTRACT_AND_RUN=1 flint
```

The AppImage is unsigned; see [Verify a download](#verify-a-download).
On Linux there is no menu bar: the command palette (Ctrl-Shift-P) has every menu item, Quit is Ctrl-Q, and Close
Window is Ctrl-Shift-W (Ctrl-W closes the active dock tab, or the window when the dock shows none). Most Cmd shortcuts
are Ctrl; [Keys](#keys) marks the exceptions. A focused shell keeps Ctrl with a letter or symbol, including Ctrl-P,
Ctrl-[, Ctrl-J, Ctrl-W and Ctrl-Q, while Ctrl-Shift shortcuts stay Flint's and Ctrl-Shift-W closes the shell tab.
Settings and state live in `~/.config/flint/` (`$XDG_CONFIG_HOME/flint/`).

### Linux MCP server only (`flint-mcp`)

Releases after v0.2.1 also have the MCP server on its own, for x86_64 and aarch64:
`flint-mcp-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz` and `flint-mcp-vX.Y.Z-aarch64-unknown-linux-musl.tar.gz`. It is
the same read-only server as `flint mcp`, without the desktop app. It is a static binary that needs no system
libraries, not even glibc, so it runs where the AppImage does not: Ubuntu 22.04 and other older distributions, minimal
containers and devcontainers. CI runs it in a bare `ubuntu:22.04`. A kubeconfig without `certificate-authority-data`
relies on the system's CA certificates, so a minimal image needs `ca-certificates` installed (or the cluster's CA in
the kubeconfig). The archive includes musl's license (`LICENSE-MUSL`), since the binary links it statically.

```sh
gh release download -R Sealos/flint --pattern 'flint-mcp-*-x86_64-unknown-linux-musl.tar.gz*'   # or aarch64
sha256sum -c flint-mcp-*-x86_64-unknown-linux-musl.tar.gz.sha256 &&
  tar -xzf flint-mcp-*-x86_64-unknown-linux-musl.tar.gz &&
  install -D -m 755 flint-mcp/flint-mcp ~/.local/bin/flint-mcp &&
  "$HOME/.local/bin/flint-mcp" --version
claude mcp add flint -- "$HOME/.local/bin/flint-mcp"
```

It reads kubeconfig as `flint` does (`KUBECONFIG`, else `~/.kube/config`, plus `kubeconfig_paths`), and its settings
from `~/.config/flint/settings.toml` (`$XDG_CONFIG_HOME/flint/`, or `$FLINT_CONFIG_DIR` if set), where it also
writes its activity log. Like `flint`, it takes `PATH` from your login shell when it starts, so exec auth plugins
(`aws`, `gke-gcloud-auth-plugin`, `kubelogin`) are found; set `path` in `settings.toml` to skip the shell. It exposes
nothing until `settings.toml` says so: see [Expose a cluster without the app](#expose-a-cluster-without-the-app).
The binary is unsigned; see [Verify a download](#verify-a-download).

### Verify a download

These are two separate checks. The `.sha256` file, used in the steps above, shows that the download is complete and
matches the file published beside it. From 0.5.0, build provenance also shows that the file was built by a workflow
in `Sealos/flint`:

```sh
gh attestation verify ./flint-vX.Y.Z-linux-x86_64.AppImage -R Sealos/flint   # use the file you downloaded
```

v0.4.0 and earlier have no build provenance, so this command fails on them. Use their checksums.

## Keys

Cmd-Shift-P opens the command palette: type part of any action's name and press Enter. It lists only what can run
now, with each action's shortcut, and asks for what an action needs (a cluster, a namespace, a kind, a column, a
label selector). Cmd-P opens the resource switcher: type part of a kind or object name ("pods web" works too) and
press Enter to open the kind's list, or the object selected with its details; it searches only what Flint already
holds in memory, so it never asks the cluster. These are the default keys on macOS; on Linux, Cmd is Ctrl, except
where the table says otherwise and in a focused shell, which keeps Ctrl with a letter or symbol.

With a log tab focused, run **Set Context Lines…** from the command palette to show 0 to 100 lines before and after
each filtered match.

| Key | Action | Where |
|---|---|---|
| ⇧⌘P | Show Command Palette (again to close it) | anywhere |
| ⌘P | Go to Resource: a kind, or an object in memory (again to close it; on Linux, Ctrl-P there moves up instead) | anywhere |
| ↑ ↓ or ⌃P ⌃N, ⏎, ⎋ | Move, run the selected action or open the kind or object, close (or go back a step) | in the palette or the switcher |
| ⇥, ⇧⇥ | Focus Next Pane, Focus Previous Pane: clusters, navigation, list, detail, dock | outside text fields and shells |
| ↑ ↓, ⏎ | Move between clusters, connect to the selected one | the cluster sidebar |
| ↑ ↓, ⏎, → ← | Move, open the kind (or open or close the section), open the section, close it | the navigation |
| ⌘[ | Go Back to the kind and object shown before | outside text fields |
| ⌘F | Focus Filter | a list |
| ↑ ↓, Home, End, ⏎ | Select the previous, next, first, last row; open the selected row | a list |
| ⇧⌘B | Back to List, keeping the selection | the detail in a narrow window |
| ⎋ | Close Detail | the detail pane |
| ⌘1, ⌘2 | Show Overview, Show YAML | the detail pane |
| ⌘R | Refresh Object; Retry Connection; Reconnect Logs or Restart Shell (Ctrl-Shift-R on Linux) | the detail pane; a failed cluster; the dock |
| ⌘L, ⇧⌘L | Open Logs, Open Previous Logs (a pod only) | the selected pod or workload |
| ⇧⌘E | Open Shell | the selected pod or workload |
| ⌘J, ⇧⌘J | Toggle Dock, Show Dock Tabs | anywhere |
| ⌃⇥, ⌃⇧⇥ | Next, Previous Dock Tab | the dock |
| ⌘W, ⇧⌘W | Close Dock Tab (the window when no tab is shown), Close Window | anywhere |
| ␣, T, P, W, M, E | Toggle Pause, Timestamps, Pod Prefix, Line Wrap, Matching Lines Only, Errors Only | a log tab outside the search field |
| ⌘F, ⌘G, ⇧⌘G | Search Logs, Next Match, Previous Match | a log tab |
| ⌘S, ⇧⌘S, ⌥⌘S | Save Logs, Save Matching Lines, Save Selected Lines | a log tab |
| ↑ ↓, ⇧↑ ⇧↓, ⌘A, ⌘C, ⎋ | Select a line, extend the selection, select all, copy, clear | a log tab |
| ⇧⇞ ⇧⇟, ⇧↖ ⇧↘ | Scroll a page, to the top or bottom | a shell tab |
| ⌘C, ⌘V | Copy, Paste (Ctrl-Shift-C, Ctrl-Shift-V on Linux; Edit > Copy and Paste too) | a shell tab |
| ⌘X, ⌘C, ⌘V, ⌘A | Cut, Copy, Paste, Select All | a text field |
| ⌘H, ⌥⌘H | Hide Flint, Hide Others (macOS only) | anywhere |
| ⌘Q | Quit Flint | anywhere |

## Build and run

You need a recent stable Rust toolchain (edition 2024) and macOS with Xcode command line tools. On Linux, install
GPUI's system libraries instead; `.github/actions/gpui-linux-deps/action.yml` lists the Ubuntu packages.

```sh
cargo build --release
cargo run -p flint-app            # start the app
```

Flint reads kubeconfig the way `kubectl` does: the files in `KUBECONFIG` if it is set, otherwise `~/.kube/config`,
plus any `kubeconfig_paths` from settings. It never writes to kubeconfig files, and it does not connect to any
cluster until you select one.

Settings live in `~/Library/Application Support/Flint/settings.toml` (or in `$FLINT_CONFIG_DIR` if set):

```toml
kubeconfig_paths = ["~/work/kubeconfig"]
# appearance = "system"   # or "light" / "dark"; View > Appearance writes it
# path = "/opt/homebrew/bin:/usr/bin:/bin"   # override the PATH taken from your login shell
# helm = "/opt/homebrew/bin/helm"
watch_mode = "all"                # or "on_view"
# protect_local_clusters = false  # true protects known local clusters too (see trusted_clusters)

[cluster_watch_mode]
# big-prod-cluster = "on_view"

# Names Flint shows for clusters, by context name (right-click a cluster to rename it). Flint only:
# kubeconfig is not changed, and AI agents and shells keep the context name.
[cluster_names]
# "arn:aws:eks:eu-west-1:123456789012:cluster/example" = "example-eu"

# Colour tags, written by a cluster's row menu: blue, purple, teal, green, yellow or red. Pinned to
# the server, so a re-pointed context shows no tag.
# [[cluster_tags]]
# context = "staging"
# server = "https://staging.example.com:6443"
# tag = "yellow"

# Clusters flint mcp may see, written by the sidebar's Expose to AI toggle. Each entry is a
# context name and the API server URL it pointed to when you exposed it; if the context later
# points elsewhere, it is hidden from agents and the sidebar shows "Changed".
# [[ai_exposed_clusters]]
# context = "kind-flint-dev"
# server = "https://127.0.0.1:6443"
# namespaces = ["shop"]          # optional: only these namespaces (empty or missing: all)
# kinds = ["pods", "deployments.apps"]  # optional: only these kinds (empty or missing: all)

# Clusters Flint may change (and open shells in) without asking, written by Trust This Cluster in
# the sidebar's lock menu. Pinned to the server like exposure: a re-pointed context is protected
# again. Known local clusters are unlocked anyway: a context named exactly kind, minikube,
# docker-desktop or orbstack, or starting with kind-, whose server is on this machine (localhost or
# *.localhost, 127.x.x.x, ::1, Docker Desktop's kubernetes.docker.internal). Any other context on
# those addresses is protected: unlock or trust it. So is a local cluster on another address, such
# as minikube's Docker driver on Linux (192.168.49.2). Known local clusters stay unlocked unless
# protect_local_clusters is true. A context whose cluster has a proxy-url is never treated as
# local. While settings.toml does not parse, nothing is trusted and local clusters are protected
# too.
# [[trusted_clusters]]
# context = "staging"
# server = "https://staging.example.com:6443"
```

The app watches `settings.toml` and your kubeconfig files and picks up changes without a restart.

Flint and `flint mcp` log to stderr and share a diagnostic file. On macOS it is `~/Library/Logs/Flint/flint.log`.
On Linux it is `$XDG_STATE_HOME/flint/flint.log`, defaulting to `~/.local/state/flint/flint.log`. The file stores
Flint's own diagnostics with credential redaction and rotates at 4 MiB, keeping three older files (`flint.log.1`
through `flint.log.3`). If the file is unavailable, logging continues on stderr. MCP uses stdout for the protocol.

By default stderr shows warnings and errors, and `flint --verbose` (or `flint mcp --verbose`) adds info. The file
records info and above by default, so a bug report has context. A valid `FLINT_LOG` takes precedence over `--verbose`
and sets the filter for both destinations, for example `FLINT_LOG=debug` or `FLINT_LOG=flint_core=debug`; `off` turns
both off. On stderr the filter applies to Flint's own events, and other crates are capped at warnings and errors
whatever it says. The file keeps Flint's own events only. Flint reads `RUST_LOG` when `FLINT_LOG` is unset, empty or
invalid. `flint --help` lists
the commands, options and environment variables, with the development aids below under their own heading.

`FLINT_CLUSTER=<context>` is a development and testing aid, for example for agents that take screenshots: at startup
Flint activates that context exactly as a click would. It must name a context in the loaded kubeconfig; otherwise Flint
logs a warning and starts with nothing selected. It never selects any other context. Use it only with the dev
cluster:

```sh
KUBECONFIG=$PWD/.dev/kubeconfig FLINT_CONFIG_DIR=$PWD/.dev/flint FLINT_CLUSTER=kind-flint-dev cargo run -p flint-app
```

Two more development aids work together with `FLINT_CLUSTER`, so screenshots can show views nobody clicked to:

- `FLINT_VIEW` opens a view once the cluster's discovery arrives: a kind (`pods`, `deployments.apps`,
  `widgets.example.com`; the group may be left out), `kind/name` for a cluster-scoped object, or
  `namespace/kind/name`. Add `#yaml` to an object to open its YAML tab, or `#logs` to a pod to open its logs.
  Without `FLINT_CLUSTER` it is ignored.
- `FLINT_APPEARANCE=light` or `dark` forces a theme over the `appearance` setting and macOS.

```sh
FLINT_CLUSTER=kind-flint-dev FLINT_VIEW=demo/deployments/web#yaml FLINT_APPEARANCE=light cargo run -p flint-app
```

## Development cluster

`scripts/dev-cluster.sh` manages a throwaway [kind](https://kind.sigs.k8s.io/) cluster named `flint-dev`, with the
fixtures the cluster tests expect (`scripts/cluster-fixtures.sh`): metrics-server, a `demo` namespace with a Deployment
and the `podinfo` Helm release, and old resourceVersions expired (etcd compacted, API server restarted). Its kubeconfig is written to `.dev/kubeconfig` (gitignored), so your own
`~/.kube/config` is never touched.

```sh
scripts/dev-cluster.sh up         # create it
export KUBECONFIG=$PWD/.dev/kubeconfig
FLINT_CONFIG_DIR=$PWD/.dev/flint cargo run -p flint-app
scripts/dev-cluster.sh down       # delete it
```

## Tests

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Tests that need a cluster are marked `#[ignore]`, so a plain `cargo test` reports them as ignored rather than passed.
To run them, pass `--include-ignored` and point `FLINT_TEST_KUBECONFIG` at the dev cluster only (they fail if it is
unset). They also call `kubectl` with that kubeconfig.

```sh
FLINT_TEST_KUBECONFIG=$PWD/.dev/kubeconfig KUBECONFIG=$PWD/.dev/kubeconfig \
  cargo test --workspace -- --include-ignored
```

They create objects only in namespaces named `m1fix-*` (and one CRD in the `m1fix.flint.dev` group),
wait for them to be deleted again, and can be run back to back. Two tests need more and skip with a
message unless asked:

- The load measurement (`tests/watch_load.rs`, UP-26) needs a prepared namespace named by
  `FLINT_LOAD_NAMESPACE`; see its doc comment. Run it in release mode with `--nocapture`.
- The API-server restart test (`tests/watch_recovery.rs`, UP-27) stops and starts the kind control-plane
  container for about half a minute. Run it on its own, with nothing else using the cluster:

  ```sh
  FLINT_TEST_KUBECONFIG=$PWD/.dev/kubeconfig FLINT_TEST_SLOW=1 \
    cargo test -p flint-core --test watch_recovery -- --ignored --nocapture
  ```

CI (`.github/workflows/ci.yml`) runs format, clippy, docs and the tests above without a cluster. Everything runs on
Linux except the app, which is the only part that needs macOS. The cluster tests run on Linux against kind
(`.github/workflows/cluster-e2e.yml`).

### Checks CI runs

On a pull request, each check runs only when the change touches what it checks: `scripts/ci-changes.sh` sorts
the changed paths into groups (Rust code, the app, the AppImage, dependencies, scripts, workflows), so a
docs-only PR skips the builds and tests and runs only cheap checks such as `typos` and the PR title. A skipped check counts as passed. Ordinary PRs run a smoke
suite: every test compiles and a named subset runs. Pushes to `master` and PRs touching the paths `needs_full` lists
in that script, including the CI files, run the full suite. See
[Which tests a pull request runs](docs/TESTING.md).

- **CI (Linux):** `cargo fmt --check`, and for `flint-core` and `flint-mcp`: clippy with `-D warnings`,
  tests as a separate job per crate, and `cargo doc` with `-D warnings`. For the app,
  `linux (flint-app)` runs its clippy and view tests, and `appimage (x86_64)` packages a release build with
  `scripts/package-appimage.sh` and smoke-tests it with `scripts/smoke-appimage.sh`.
- **CI (macOS), one job, `macos (flint-app)`:** the app's clippy, view tests and docs, and a release build. It runs on
  every push to `master`, on PRs labelled `run-macos`, and on every full-suite PR. macOS
  runners cost about ten times as much as Linux ones, so only work that needs macOS runs there.
- **macOS packaging, `macos-app (dmg)`:** on PRs that touch the DMG packaging, packages `Flint.app` and its DMG and
  runs `scripts/smoke-macos-app.sh` and its self-test (`scripts/smoke-macos-app-test.sh`). Not required.
- **Cluster e2e (Linux):** a kind cluster set up by `scripts/cluster-fixtures.sh`, then the `flint-core` and
  `flint-mcp` tests with `--include-ignored`, the MCP ones through the GPUI-free `flint-mcp` binary. Runs on PRs that
  touch core or MCP, on master, and nightly (which adds the API-server restart test).
- **Lint (Linux):**
  - `cargo deny check` (advisories, licenses, bans, sources; config in `deny.toml`);
  - `cargo machete` (unused dependencies);
  - `typos`;
  - `actionlint`;
  - `shellcheck` on `scripts/*.sh`;
  - `scripts/check-module-tests.sh`;
  - `scripts/check-theme-colors.sh` (no hard-coded colors in UI code; see below).
- **PR title:** must be a Conventional Commit, because PRs are squash-merged and release-please reads the subject.
- **Dependabot:** weekly Cargo and GitHub Actions updates. `gpui-kit` is excluded because it is pinned on purpose.

**Unit tests per module:** every source module has unit tests, either a `#[cfg(test)] mod tests` or a
`// module-tests: <file>` line pointing to its test file. `lib.rs`, `main.rs` and `mod.rs` are wiring and are exempt.
`scripts/module-tests-allowlist.txt` lists the modules that predate this rule. The list may only shrink.

**Theme tokens only:** views take every color from the theme (`cx.theme()`). Colors live only in
`crates/flint-app/src/ui/themes/flint.json` (Flint Light and Flint Dark) and `ui/accent.rs` (the orange accent).
`scripts/check-theme-colors.sh` fails on a color literal anywhere else under `ui/`.

## Releases

Releases are cut by [release-please](https://github.com/googleapis/release-please) from
[Conventional Commits](https://www.conventionalcommits.org/) on `master`. Commit subjects and PR titles follow that
format, for example `feat(mcp): …` or `fix(core): …`.

The type decides what the release notes show. `feat`, `fix` and `perf` are listed; `refactor`, `docs`, `test`, `ci`,
`build`, `chore` and `style` are hidden (`release-please-config.json`). Use `feat` only for something a person using
Flint can see or do. Internal work, such as an action registry, a test-only automation channel or a fake exec
server, is `refactor`, `test` or `chore` (or `fix` when it repairs a bug a user could hit), even when it adds code.

1. Every push to `master` updates an open **release PR** with the next version and the `CHANGELOG.md` entries.
   Below 1.0, `feat` bumps the minor version and `fix` bumps the patch.
2. Merging the release PR tags `vX.Y.Z` and creates the GitHub release as a draft. Releases are immutable once
   published, so the draft stage is when files can still be attached.
3. The same workflow then builds `flint` for Apple Silicon and Intel, combines them into a universal binary, and
   packages `flint-vX.Y.Z-macos-universal.tar.gz`. It also packages that binary as an ad-hoc signed `Flint.app` in
   `flint-vX.Y.Z-macos-universal.dmg` (`scripts/package-macos-app.sh`) and mounts the DMG to check it
   (`scripts/smoke-macos-app.sh`). In parallel it builds an AppImage on x86_64 and aarch64 Ubuntu
   runners (`scripts/package-appimage.sh`): `flint-vX.Y.Z-linux-x86_64.AppImage` and
   `flint-vX.Y.Z-linux-aarch64.AppImage`, and the MCP server alone as a static musl binary on the same runners
   (`scripts/package-mcp-linux.sh`): `flint-mcp-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz` and
   `flint-mcp-vX.Y.Z-aarch64-unknown-linux-musl.tar.gz`, each run in a bare `ubuntu:22.04` with the MCP e2e tests.
   Each binary also gets a CycloneDX SBOM, and every file comes with its SHA-256 checksum. The build jobs hold a
   read-only token. An `attest` job records build provenance for those 24 files, then a separate `upload` job
   attaches them to the draft.
4. Once every upload has succeeded, a `publish` job checks that all 24 files are on the draft and match their
   checksums, then publishes the release.

Until the first signed release, release binaries are **unsigned** (the app only ad-hoc signed). macOS blocks the
first launch of a browser download until you allow it ([Install from a release](#install-from-a-release)). AppImages
and the `flint-mcp` archives stay unsigned.

After every published release:

1. **The release checklist** ([`docs/RELEASING.md`](docs/RELEASING.md)): the release page is Latest with all six
   binaries and their checksums, and the DMG installs and opens on a clean Mac (UP-80).
2. **Reviews of the shipped build:** usability (install and use it as a new user), product development (is the
   roadmap right?) and market fit (against Lens, Freelens, k9s, Headlamp and other tools). Their findings become
   issues.
3. **A refactor pass:** behaviour-preserving `refactor(<scope>): …` PRs that work through the
   [refactor backlog](https://github.com/Sealos/flint/issues/87) and the duplication the release added. Existing
   tests must pass unchanged; add tests first where coverage is thin.

Each milestone also ends with a release review (correctness, UI/UX, performance, feature set, and tests against
[`docs/USER-PATHS.md`](docs/USER-PATHS.md)) before its release is cut.

The version lives in the workspace `Cargo.toml` (marked `x-release-please-version`). The workflow refreshes
`Cargo.lock` on the release PR, because CI builds with `--locked`.

## Documentation

- [Website](https://sealos.github.io/flint/): the static site in `site/`, published by `.github/workflows/pages.yml`
- [Architecture](docs/ARCHITECTURE.md)
- [Run `flint-mcp` in a Kubernetes pod](docs/FLINT-MCP-IN-POD.md)
- [Release checklist](docs/RELEASING.md)
- [Architecture decision records](docs/adr/)

## License

Apache-2.0. See [LICENSE](LICENSE).
