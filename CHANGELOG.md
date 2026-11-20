# Changelog

## [0.2.1](https://github.com/Sealos/flint/compare/v0.2.0...v0.2.1) (2026-11-20)


### Features

* **app:** command palette and standard macOS menus as registry actions ([#220](https://github.com/Sealos/flint/issues/220)) ([8eea2ba](https://github.com/Sealos/flint/commit/8eea2ba6424fb965bd3afef9f884a27feae54828))
* **app:** protect clusters by default and gate exec ([#234](https://github.com/Sealos/flint/issues/234)) ([f62a2a8](https://github.com/Sealos/flint/commit/f62a2a87d182cc8abb348fd9050c2b56f6dafed5))
* **app:** remember each list's sort per cluster and kind ([#237](https://github.com/Sealos/flint/issues/237)) ([e853061](https://github.com/Sealos/flint/commit/e853061e64532f12c4cacb1ac728c8350e340a57))
* **cli:** list commands and environment variables in --help, quiet logs by default ([#230](https://github.com/Sealos/flint/issues/230)) ([a1fb27b](https://github.com/Sealos/flint/commit/a1fb27bc0521fe94f62f76551a0c676bc5cdffb6))
* **mcp:** redact credentials in pod logs and events served to agents ([#221](https://github.com/Sealos/flint/issues/221)) ([0a69419](https://github.com/Sealos/flint/commit/0a694192d3e92ef2d86e27ff80e27dcae94d70c0))
* **release:** ship Flint.app in a DMG with each release ([#193](https://github.com/Sealos/flint/issues/193)) ([85996d7](https://github.com/Sealos/flint/commit/85996d763933e7da03ceec9f4202e51bbfc15505))
* **site:** switch the app icon to the flat faceted stone ([#233](https://github.com/Sealos/flint/issues/233)) ([dfe2adf](https://github.com/Sealos/flint/commit/dfe2adf57d67c89613c77ea6f818106014689e5e))


### Bug Fixes

* **app:** close the dock tab on Cmd-W, and ask before closing or quitting while sessions run ([#206](https://github.com/Sealos/flint/issues/206)) ([b6717e2](https://github.com/Sealos/flint/commit/b6717e2ee9771e8214601ac6001201cb234fe8c3))
* **app:** palette context matching, popups and live choices ([#225](https://github.com/Sealos/flint/issues/225)) ([288f3fb](https://github.com/Sealos/flint/commit/288f3fbce2c7db2e48ed3ab3a96ba5d6b2799112))
* **app:** release a closed view's focused text field ([#176](https://github.com/Sealos/flint/issues/176)) ([2e121f9](https://github.com/Sealos/flint/commit/2e121f92cf0fcb35a87090ed90ce56e8837cb47a))
* **e2e:** count only requests on connections opened after the app launched ([#242](https://github.com/Sealos/flint/issues/242)) ([c34ab39](https://github.com/Sealos/flint/commit/c34ab39c1988662c7cf63ed036f7bc2244493c6e))
* **mcp:** close credential redaction gaps from [#224](https://github.com/Sealos/flint/issues/224) ([#235](https://github.com/Sealos/flint/issues/235)) ([15daf11](https://github.com/Sealos/flint/commit/15daf1127566a32c130d27383750b0e0aa36b241))
* **mcp:** close redaction gaps in dsn delimiters, set-cookie arrays and single-quoted cookies ([#240](https://github.com/Sealos/flint/issues/240)) ([84eb3e1](https://github.com/Sealos/flint/commit/84eb3e1b2cb25e74c9ddaa94e5fe7dc608964d2d))
* **mcp:** refuse names that are not single path segments, and redact secret lists without item kinds ([#243](https://github.com/Sealos/flint/issues/243)) ([dd6b6cc](https://github.com/Sealos/flint/commit/dd6b6cc2a6ad48cbed45ba5ff8385f1dea90bb40))

## [0.2.0](https://github.com/Sealos/flint/compare/v0.1.1...v0.2.0) (2026-11-20)


### Features

* **app:** action registry for every interactive control ([#64](https://github.com/Sealos/flint/issues/64)) ([8d6251f](https://github.com/Sealos/flint/commit/8d6251fca2bdb2f5f9d0cca0d8a6247b7ab0ae81))
* **app:** automation channel for test builds ([#92](https://github.com/Sealos/flint/issues/92)) ([fd71eff](https://github.com/Sealos/flint/commit/fd71eff125a81b7fc7cf8c9d745cd054e32404ca))
* **app:** bottom dock and pod log viewer with save to file ([#95](https://github.com/Sealos/flint/issues/95)) ([59aac98](https://github.com/Sealos/flint/commit/59aac98ba1b52a41e2e2b5b17da46c86083a4168)), closes [#75](https://github.com/Sealos/flint/issues/75)
* **app:** exec terminal tab in the dock ([#106](https://github.com/Sealos/flint/issues/106)) ([f8b1af6](https://github.com/Sealos/flint/commit/f8b1af6d96d563a93cb2110ce6caf2d463b701f1)), closes [#14](https://github.com/Sealos/flint/issues/14)
* **app:** open logs and a shell from list rows ([#146](https://github.com/Sealos/flint/issues/146)) ([194a79c](https://github.com/Sealos/flint/commit/194a79c5bcad2c70e9cca609712172b47d4ffd5e))
* **core:** add pod exec sessions over websocket ([#72](https://github.com/Sealos/flint/issues/72)) ([5b398d6](https://github.com/Sealos/flint/commit/5b398d6f2398f9165ec2fa828bac7dec9123108f))
* **core:** follow logs across a workload's pods ([#91](https://github.com/Sealos/flint/issues/91)) ([2f3e052](https://github.com/Sealos/flint/commit/2f3e052310353ec0cb66126c2d31a75625a43f35))
* **core:** stream pod logs into a ring buffer with search and severity ([#71](https://github.com/Sealos/flint/issues/71)) ([0407b89](https://github.com/Sealos/flint/commit/0407b893ffbf456877b73cd0c709befc4054da77))
* **e2e:** fake exec WebSocket with a scripted shell ([#168](https://github.com/Sealos/flint/issues/168)) ([b441483](https://github.com/Sealos/flint/commit/b4414836fbc49af39ec1f1f901f05720f6b5e87f))
* **release:** ship a Linux AppImage ([#94](https://github.com/Sealos/flint/issues/94)) ([35821b6](https://github.com/Sealos/flint/commit/35821b6399525f179d05faccda44fdd1411e5c71))
* **site:** obsidian stone app icon, favicon and wordmark ([#99](https://github.com/Sealos/flint/issues/99)) ([4965fb8](https://github.com/Sealos/flint/commit/4965fb829c4e856d8bf1984b40e7d2a686dcd3ed))
* **ui:** color pod status cells and keep the list readable in narrow windows ([#61](https://github.com/Sealos/flint/issues/61)) ([7c04c57](https://github.com/Sealos/flint/commit/7c04c57fc47ee672f5b14a0fa201465d371c84e0))


### Bug Fixes

* **app:** a one-row log toolbar, plain errors, short timestamps and per-pod prefixes ([b12ce93](https://github.com/Sealos/flint/commit/b12ce930703e9e2c31dcfc095a2d230fb001221a))
* **app:** container pickers for Logs, reasons under the pane's buttons, plain refusals ([ddc874d](https://github.com/Sealos/flint/commit/ddc874de05e0aad977ce73d1e9ae5600d19280cc))
* **app:** dock tabs that stay in view, name their cluster and restart ([5dd5a50](https://github.com/Sealos/flint/commit/5dd5a50696b55689bede82c775736fd79a0727a6))
* **app:** remove only an automation socket, not a file that reused its inode ([#163](https://github.com/Sealos/flint/issues/163)) ([c3f478d](https://github.com/Sealos/flint/commit/c3f478d6ad740da20c01fb1f2eb29271b41aa16e))
* **core:** close the exec connection when a shell session ends ([1100737](https://github.com/Sealos/flint/commit/11007370f5e43e7057111899939bb4c21a56f7c3))
* **core:** exec ends on a broken connection, TCP keepalive, and plain log lines ([0bcdf69](https://github.com/Sealos/flint/commit/0bcdf695e260978899709bd7b1b5d2a84ed4b37b))
* **core:** follow a pod that matches the selector again ([#96](https://github.com/Sealos/flint/issues/96)) ([2858ab2](https://github.com/Sealos/flint/commit/2858ab2686d8dce1a5495d19ba5d1316d6607512))
* **core:** jitter watch reconnect backoff on macOS ([#55](https://github.com/Sealos/flint/issues/55)) ([77b7bab](https://github.com/Sealos/flint/commit/77b7babdd6884f6b28b8ee851d3bb426479d9048))
* **core:** resolve kind short names and name the failing endpoint in errors ([#77](https://github.com/Sealos/flint/issues/77)) ([b804ad9](https://github.com/Sealos/flint/commit/b804ad9c56b454df8b4463b880f017db05e86e61))
* **core:** say network, TLS and HTTP errors in plain words ([#142](https://github.com/Sealos/flint/issues/142)) ([a2b35c2](https://github.com/Sealos/flint/commit/a2b35c2cf572dab73a1df9772a874796837e390a))
* **core:** say the pod was deleted when a pod log ends on a graceful delete ([#158](https://github.com/Sealos/flint/issues/158)) ([96c9686](https://github.com/Sealos/flint/commit/96c96862253720ac983b58700bafe0ebbb77909c))
* **ui:** lay out list columns again when a snapshot arrives ([#144](https://github.com/Sealos/flint/issues/144)) ([ac1c916](https://github.com/Sealos/flint/commit/ac1c91602b668356201468c9e312df8de66ae668))
* **ui:** release the workspace from automation and forget stale discovery ([#145](https://github.com/Sealos/flint/issues/145)) ([3a04853](https://github.com/Sealos/flint/commit/3a04853e06dbd2a44f6a960bc334b22cde977df1))
* **ui:** sidebar, list, navigation and empty-state polish from e2e screenshots ([#118](https://github.com/Sealos/flint/issues/118)) ([057b773](https://github.com/Sealos/flint/commit/057b77374785cf63a14b7970531d4f4f8074731c))


### Performance

* count selected log lines in constant time and copy them off the UI thread ([604b16d](https://github.com/Sealos/flint/commit/604b16df71654c74313fa83a319111b8930fd2d0))
* faster log search, and scan hidden tabs only when shown ([604b16d](https://github.com/Sealos/flint/commit/604b16df71654c74313fa83a319111b8930fd2d0))

## [0.1.1](https://github.com/Sealos/flint/compare/v0.1.0...v0.1.1) (2026-11-19)


### Features

* **ui:** add Flint Light and Flint Dark themes with orange accent and appearance setting ([#52](https://github.com/Sealos/flint/issues/52)) ([3cb2a77](https://github.com/Sealos/flint/commit/3cb2a77882d3329b08235c8f13f0b8d3a4e333d0))


### CI

* publish releases after attaching binaries ([#49](https://github.com/Sealos/flint/issues/49)) ([f71faad](https://github.com/Sealos/flint/commit/f71faad66c4fea094c0ff9d3865a30663aa84395))

## 0.1.0 (2026-11-19)


### Features

* resource browsing with live lists, navigation and detail view ([#45](https://github.com/Sealos/flint/issues/45)) ([4e841b1](https://github.com/Sealos/flint/commit/4e841b159c853c6d28b2be6c7f3981c7e973f8aa))
