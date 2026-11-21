# Ship a Linux AppImage alongside the macOS binary

Flint was planned as macOS first ([ADR-0001](./0001-rust-and-gpui.md)), with Linux builds a v1 non-goal. GPUI and
everything below it already build and run on Linux, and CI builds and tests the app there, so shipping Linux costs
little. Each release therefore also ships one **AppImage** per architecture, `flint-vX.Y.Z-linux-x86_64.AppImage`
and `flint-vX.Y.Z-linux-aarch64.AppImage`, each with a `.sha256`, next to the macOS universal tarball.

- **One file, any distribution.** An AppImage is a single executable: `chmod +x` and run it. `./Flint.AppImage mcp`
  passes its arguments to `flint`, so AI agents register the AppImage path the same way they register the macOS
  binary.
- **What is bundled.** `scripts/package-appimage.sh` runs linuxdeploy, which copies the shared libraries the binary
  links and a desktop may lack (for example `libxkbcommon-x11`), and leaves out what must come from the host: glibc,
  GL and Vulkan drivers, fontconfig and freetype, libxcb (linuxdeploy's excludelist). Vulkan is loaded at run time
  and is never bundled. The binary links none of the excluded libraries except libxcb, so that is the one package a
  minimal system adds, listed in `packaging/linux/ubuntu-runtime-packages.txt` and
  `packaging/linux/rhel-runtime-packages.txt`; CI and the release run the AppImage in clean AlmaLinux 9, Ubuntu 22.04,
  Debian 12 and Ubuntu 24.04 containers with only those installed. appimagetool then wraps the AppDir with the AppImage type-2 runtime. All three tools are official
  releases from the linuxdeploy and AppImage GitHub organizations, pinned by version and SHA-256 in the script.
  The runtime is MIT-licensed and embedded in every AppImage, so its notice
  (`packaging/linux/appimage-runtime-LICENSE`, checked against the pinned tag) ships in the AppDir next to Flint's
  `LICENSE`; linuxdeploy copies the bundled libraries' copyright files.
- **Built natively, on the floor.** x86_64 runs on `ubuntu-24.04` and aarch64 on `ubuntu-24.04-arm`, each building
  and packaging inside AlmaLinux 9 pinned by digest (`scripts/build-linux-appimage.sh`); linuxdeploy inspects the
  binary with the container's loader, so there is no cross-compiling. CI packages and smoke-tests both AppImages when
  the Rust code or the packaging changes (`--version` and `mcp`, without FUSE).
- **The MCP server alone, static.** Agents often run where the AppImage cannot: a distribution older than glibc
  2.34, or a container without libxcb. Each release therefore also ships `flint-mcp`, the GPUI-free
  server, as `flint-mcp-vX.Y.Z-<arch>-unknown-linux-musl.tar.gz` for x86_64 and aarch64. It is linked statically
  against musl, so it needs no library from the host, glibc included; rustls with ring is the only C code in its tree,
  and the runner's `musl-gcc` (Debian's `musl-tools`) compiles it. It is built natively on the same runners as the
  AppImages (`scripts/package-mcp-linux.sh`, which fails if the binary asks for a program interpreter or a shared
  library), then run in a bare `ubuntu:22.04` with no packages added: `--version`, `initialize`, and the MCP e2e
  tests against the fake API server, built for musl too (`scripts/test-mcp-linux.sh`). Without the app there is no
  Expose to AI dialog, so clusters are exposed by an `ai_exposed_clusters` entry in `settings.toml`, pinned to context
  and server like the one the dialog writes (README, "Expose a cluster without the app").
- **Publishing.** The macOS and Linux build jobs upload to the draft release; a separate `publish` job publishes it
  only after all of them succeeded, and checks every asset is on the published release.
- **Unsigned.** AppImages are not signed (there is no signing key to manage); the `.sha256` files are the
  integrity check, as for the macOS tarball before M6.

## Considered Options

- **deb and rpm packages:** native for their families, but two formats, each with its own dependency names and
  repository story, and still nothing for Arch or others. Worth adding if users ask for an apt or dnf repository.
- **Flatpak (Flathub):** sandboxed, which fights what Flint needs: the user's kubeconfig files, the login shell's
  `PATH`, and exec plugins, `helm` and `claude` from the host. Each would need a portal or a sandbox hole.
- **Snap:** the same sandbox problem under classic confinement review, and tied to Canonical's store.
- **AUR package:** a good fit for Arch users, but community-maintained by nature; it can repackage the AppImage or
  build from source later.
- **Tarball of the bare binary only:** simplest, but it relies on every GPUI library being installed, has no
  desktop entry or icon, and gives no single file to run.

## Consequences

- **Best effort.** Linux is supported on a best-effort basis. CI builds, tests (headless GPUI) and packages the app
  on Linux and starts it headless on four distributions, but nothing tests it on a real X11 or Wayland desktop. Bugs
  that only show on a desktop are found by users.
- **glibc floor: 2.34.** The binary links the glibc of the AlmaLinux 9 build image, so distributions older than that
  cannot run it. The image is pinned by digest, and `scripts/package-appimage.sh` fails the build if any ELF file in
  the AppDir asks for a newer glibc symbol version, so the floor cannot rise silently. The static `flint-mcp` has no
  floor, so agents on older systems run that instead of `flint mcp`.
- **musl's allocator.** The static `flint-mcp` uses musl's `malloc`, which is slower than glibc's under many
  threads. The server is mostly waiting on the API server, so this has not mattered; a faster allocator can be
  linked in if it does.
- **No menu bar.** GPUI on Linux shows no application menu. Quit and Close Window are on their keys (Ctrl-Q,
  Ctrl-W); every default key uses GPUI's `secondary-` modifier, which is Cmd on macOS and Ctrl on Linux. The View >
  Appearance choice is the `appearance` setting in `settings.toml` until the command palette lands; then every
  menu item is in the palette.
- **Window decorations.** GPUI asks the compositor for server-side decorations. Compositors that do not draw them
  (GNOME on Wayland) show the window without a title bar; Flint draws none of its own yet.
- **XDG directories.** On Linux `settings.toml` and `state.toml` live in `$XDG_CONFIG_HOME/flint` (default
  `~/.config/flint`). State stays beside settings rather than in `$XDG_STATE_HOME`, so one directory holds all of
  Flint's files on every platform and `FLINT_CONFIG_DIR` keeps overriding both. Linux was never released before
  this; source builds from before it used the macOS path (`~/Library/Application Support/Flint` under `$HOME`).
  Their users can move `settings.toml` and `state.toml` to `~/.config/flint/`, or set `FLINT_CONFIG_DIR`. Flint
  does not migrate them.
- **Extract-and-run without FUSE.** With `APPIMAGE_EXTRACT_AND_RUN=1` the runtime extracts to
  `$TMPDIR/appimage_extracted_<md5 of the AppImage>` and deletes that directory when the process exits, so two
  launches sharing a `TMPDIR` (the app and `flint mcp`, say) can delete each other's files. The README tells users
  to give each launch its own `TMPDIR`, with a ready MCP registration line; the smoke test does the same.
- **No auto-update or desktop integration.** The AppImage carries no update information, and Flint does not install
  a desktop entry; tools such as AppImageLauncher or Gear Lever do that if the user wants it.
- **Icon.** The icon in `packaging/linux/` is a placeholder (the site's favicon), kept as one SVG plus the PNG
  rendered from it, so a new logo is a file replacement.
