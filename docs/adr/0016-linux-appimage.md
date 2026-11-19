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
  GL and Vulkan drivers, fontconfig and freetype (linuxdeploy's excludelist). Vulkan is loaded at run time and is
  never bundled. appimagetool then wraps the AppDir with the AppImage type-2 runtime. All three tools are official
  releases from the linuxdeploy and AppImage GitHub organizations, pinned by version and SHA-256 in the script.
- **Built natively.** The release builds x86_64 on `ubuntu-24.04` and aarch64 on `ubuntu-24.04-arm`; linuxdeploy
  inspects the binary with the host's loader, so there is no cross-compiling. CI packages and smoke-tests the x86_64
  AppImage on every pull request (`--version` and `mcp`, without FUSE).
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
  on Linux, but nothing tests it on a real X11 or Wayland desktop, or across distributions. Bugs that only show on a
  desktop are found by users.
- **glibc floor.** The binary links the glibc of the build runner (Ubuntu 24.04, glibc 2.39), so older
  distributions cannot run it. The runners are pinned to 24.04 rather than `ubuntu-latest`, which would raise the
  floor silently when GitHub moves it. Building on an older image would lower the floor.
- **No menu bar.** GPUI on Linux shows no application menu. Quit and Close Window are on their keys (Ctrl-Q,
  Ctrl-W); every default key uses GPUI's `secondary-` modifier, which is Cmd on macOS and Ctrl on Linux. The View >
  Appearance choice is the `appearance` setting in `settings.toml` until the command palette lands; then every
  menu item is in the palette.
- **Window decorations.** GPUI asks the compositor for server-side decorations. Compositors that do not draw them
  (GNOME on Wayland) show the window without a title bar; Flint draws none of its own yet.
- **XDG directories.** On Linux `settings.toml` and `state.toml` live in `$XDG_CONFIG_HOME/flint` (default
  `~/.config/flint`). State stays beside settings rather than in `$XDG_STATE_HOME`, so one directory holds all of
  Flint's files on every platform and `FLINT_CONFIG_DIR` keeps overriding both.
- **No auto-update or desktop integration.** The AppImage carries no update information, and Flint does not install
  a desktop entry; tools such as AppImageLauncher or Gear Lever do that if the user wants it.
- **Icon.** The icon in `packaging/linux/` is a placeholder (the site's favicon), kept as one SVG plus the PNG
  rendered from it, so a new logo is a file replacement.
