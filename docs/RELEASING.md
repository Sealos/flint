# Release checklist

What a person checks after a release is published. Cutting the release is automatic (see
[Releases](../README.md#releases)): merging the release PR tags it, the `release-please` workflow builds and attaches
every file, and its `publish` job publishes the release. The checks below catch what that workflow cannot see: a
release page that looks wrong, and the first launch of the app on a Mac that has never run it.

Work through it for every published release, top to bottom, and note the result on the release PR. A failed step
becomes an issue before anything else.

## 1. The release page

Open the release on [GitHub Releases](https://github.com/Sealos/flint/releases), or run
`gh release view vX.Y.Z -R Sealos/flint`.

- [ ] It is published: not a draft and not a pre-release.
- [ ] It is marked **Latest**, and [/releases/latest](https://github.com/Sealos/flint/releases/latest) opens it. The
  `publish` job only marks a release Latest when no newer one is out, so an older release re-published later is
  correctly not Latest.
- [ ] It has eight files, four binaries and their `.sha256` checksums, besides GitHub's two Source code archives:
  - `flint-vX.Y.Z-macos-universal.dmg` and `.dmg.sha256`
  - `flint-vX.Y.Z-macos-universal.tar.gz` and `.tar.gz.sha256`
  - `flint-vX.Y.Z-linux-x86_64.AppImage` and `.AppImage.sha256`
  - `flint-vX.Y.Z-linux-aarch64.AppImage` and `.AppImage.sha256`

  v0.2.0 and earlier have no DMG.
- [ ] The notes list the changes from `CHANGELOG.md`.
- [ ] The checksums match. In an empty directory:

  ```sh
  gh release download vX.Y.Z -R Sealos/flint
  shasum -a 256 -c ./*.sha256
  ```

The `publish` job already checks that all eight files are on the draft, non-empty and matching their checksums, and
that the release ends up published (and Latest when it should be). This step confirms what someone downloading it
sees.

## 2. Install from the DMG on a clean Mac (UP-80)

[UP-80](USER-PATHS.md#up-80-install-from-the-dmg) asks that the DMG installs and opens on a Mac that has never run
Flint, on Apple Silicon and on Intel. CI mounts and checks the DMG (`scripts/smoke-macos-app.sh`), but only a person
can go through Gatekeeper's first-launch path.

**A clean Mac** is a macOS virtual machine or a new macOS user account with no Flint installed before: no
`/Applications/Flint.app`, no `~/Library/Application Support/Flint/`. Run the check once on Apple Silicon and once on
Intel. Note the macOS version, since the first-launch steps differ between macOS versions.

**Never use `~/.kube/config` for this check.** Use a scratch kubeconfig, for example a copy of the kind dev cluster's
`.dev/kubeconfig` (see [Development cluster](../README.md#development-cluster)), and a scratch `FLINT_CONFIG_DIR`.
Opened from Finder, Flint reads `~/.kube/config` and ignores a `KUBECONFIG` set in a shell, so the clean Mac should
have no `~/.kube/config`. If it has one, do not select any of its clusters: Flint connects to nothing until a cluster
is selected.

- [ ] **Download with a browser.** In Safari, download `flint-vX.Y.Z-macos-universal.dmg` and its `.sha256` from the
  release page. A browser download is quarantined, which is what puts it through Gatekeeper; a copy downloaded with
  `gh` or `curl` is not and skips the check. Verify it in the Downloads folder:

  ```sh
  cd ~/Downloads && shasum -a 256 -c flint-vX.Y.Z-macos-universal.dmg.sha256
  ```

- [ ] **Install.** Open the DMG. It shows **Flint** and an **Applications** link. Drag Flint onto Applications, then
  eject the DMG.
- [ ] **First launch.** Open Flint from Applications. Until the first signed release the app is ad-hoc signed only,
  so macOS refuses it the first time. Check that one of the README's
  [first-launch steps](../README.md#macos-app-dmg) gets past it, and note which one you used:
  - dismiss the warning (**Done**, not Move to Trash), open **System Settings > Privacy & Security**, click **Open
    Anyway** next to the message about Flint, and confirm; or
  - remove the quarantine flag: `xattr -dr com.apple.quarantine /Applications/Flint.app`, then open Flint again.

  On macOS 15 and later Control-click > Open no longer gets past this, so it is not a valid path. Once a release is
  signed and notarized (#25), UP-80 expects no warning at all, and any warning fails the check.
- [ ] **It starts.** The window opens, and **Flint > About Flint** shows version X.Y.Z. Quit it.
- [ ] **It lists clusters from a scratch kubeconfig.** Copy a scratch kubeconfig to the clean Mac, then start the app
  binary from a terminal, so it reads that kubeconfig and keeps its settings in a scratch directory:

  ```sh
  scratch=$(mktemp -d)
  cp /path/to/scratch-kubeconfig "$scratch/kubeconfig"
  KUBECONFIG="$scratch/kubeconfig" FLINT_CONFIG_DIR="$scratch/flint" /Applications/Flint.app/Contents/MacOS/flint
  ```

  The sidebar lists the scratch kubeconfig's contexts (`kind-flint-dev` for a copy of `.dev/kubeconfig`). Listing
  them needs no connection. If the cluster is reachable from this Mac, for example a kind dev cluster running on it,
  select it and check that it connects and its lists fill in.
- [ ] **The command-line binary works.** `/Applications/Flint.app/Contents/MacOS/flint --version` prints
  `flint X.Y.Z`.
- [ ] Clean up: delete `/Applications/Flint.app`, `~/Library/Application Support/Flint/` (written by the launch from
  Finder), `$scratch`, and the VM or user account if it was made for this. Do the next section first if you run it.

Record on the release PR: macOS version and chip for each run, the first-launch path used, and any warning text that
differed from the README. If the steps no longer match what macOS shows, fix the README's
[first-launch steps](../README.md#macos-app-dmg) in the same pass.

## 3. Exec-plugin auth from Finder (UP-05)

[UP-05](USER-PATHS.md#up-05-exec-plugin-auth-when-launched-from-finder) needs a real cloud cluster (EKS, GKE or one
that uses `kubelogin`), so it is checked by hand. Run it on the clean Mac after section 2, when a cloud cluster you may
use for testing is at hand, and always when a release changes how Flint reads `PATH`.

- [ ] Install the cloud's exec plugin (`aws`, `gke-gcloud-auth-plugin` or `kubelogin`) so that it is on the `PATH` set
  in your shell rc files only, not in `/usr/bin` or `/opt/homebrew/bin`.
- [ ] Write a scratch kubeconfig with only that cloud context, and add it to
  `~/Library/Application Support/Flint/settings.toml`: `kubeconfig_paths = ["/path/to/scratch-kubeconfig"]`. Do not
  use `~/.kube/config`.
- [ ] Open Flint from Finder or the Dock, select the cluster, and check that it connects.

## 4. After the checks

The reviews and the refactor pass that follow every release are listed under [Releases](../README.md#releases).
