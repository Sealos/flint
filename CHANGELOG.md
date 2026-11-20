# Changelog

## [0.3.0](https://github.com/Sealos/flint/compare/v0.2.1...v0.3.0) (2026-11-20)


### Features

* **app:** resource switcher, pane keys and go back ([dec65e1](https://github.com/Sealos/flint/commit/dec65e16bdd53487b543254427acfb52b7715c90))
* **app:** resource switcher, pane keys and go back ([#244](https://github.com/Sealos/flint/issues/244)) ([dec65e1](https://github.com/Sealos/flint/commit/dec65e16bdd53487b543254427acfb52b7715c90))
* **logs:** show only matching lines, an errors-only view and context lines ([#291](https://github.com/Sealos/flint/issues/291)) ([fd7b8a6](https://github.com/Sealos/flint/commit/fd7b8a686c3052881347a813b599e717ffb1cc1e))
* **mcp:** limit an exposed cluster to chosen namespaces and kinds ([#241](https://github.com/Sealos/flint/issues/241)) ([7bc7c7f](https://github.com/Sealos/flint/commit/7bc7c7f2fa8804a4996998248d0281cf6acb8042))
* **mcp:** record every agent call and show it per cluster ([#286](https://github.com/Sealos/flint/issues/286)) ([b4e4d7d](https://github.com/Sealos/flint/commit/b4e4d7de754e21086e8c618fb5180c2ab2306ea6)), closes [#69](https://github.com/Sealos/flint/issues/69)
* write a rotated log file, and ask for it in the bug template ([#312](https://github.com/Sealos/flint/issues/312)) ([d476ae8](https://github.com/Sealos/flint/commit/d476ae81636e41ff2f602fad555c7bd3ca5adcf7))


### Bug Fixes

* **app:** esc leaves the list filter, and go back skips the current place ([#317](https://github.com/Sealos/flint/issues/317)) ([c9c4949](https://github.com/Sealos/flint/commit/c9c49498efbe3bba187e33dcc363692cdf2c7bfa))
* **core:** ask for no events once a detail read's deadline is spent ([#284](https://github.com/Sealos/flint/issues/284)) ([cdcb39a](https://github.com/Sealos/flint/commit/cdcb39a73cbbbb5f9a3918b4f1f6f9761fec6a61))
* **core:** cancel the detail fetch when its pane closes ([#281](https://github.com/Sealos/flint/issues/281)) ([669be33](https://github.com/Sealos/flint/commit/669be335e0dc04754455449487b905fdfdf5e10a)), closes [#258](https://github.com/Sealos/flint/issues/258)
* **core:** context pin follow-ups for exec and log sessions ([#303](https://github.com/Sealos/flint/issues/303)) ([fba19ad](https://github.com/Sealos/flint/commit/fba19ad0f54148b21e85e858d18a4f16fcc2b08b))
* **core:** end an exec session when its context is re-pointed to another server ([#290](https://github.com/Sealos/flint/issues/290)) ([57dcb41](https://github.com/Sealos/flint/commit/57dcb41ca1675a0c22db6ac15968daa5ef2c8505)), closes [#260](https://github.com/Sealos/flint/issues/260)
* **core:** exec plugin terminal edge cases after [#283](https://github.com/Sealos/flint/issues/283) ([#293](https://github.com/Sealos/flint/issues/293)) ([d7528a3](https://github.com/Sealos/flint/commit/d7528a37272bfdde234d0240fbdc5b636502a94c)), closes [#288](https://github.com/Sealos/flint/issues/288)
* **core:** keep exec plugin queue order, skip the login cooldown when the plugin never started ([#318](https://github.com/Sealos/flint/issues/318)) ([1e63ec5](https://github.com/Sealos/flint/commit/1e63ec518ec604c0f9b302ea492ce395b777fb1d))
* **core:** keep pruning the watch interner during steady churn ([#282](https://github.com/Sealos/flint/issues/282)) ([e599423](https://github.com/Sealos/flint/commit/e599423787471dc86522ecba226b26bf1c915f22)), closes [#257](https://github.com/Sealos/flint/issues/257)
* **core:** kill exec auth plugins that outlive the connect timeout ([#283](https://github.com/Sealos/flint/issues/283)) ([cf2098d](https://github.com/Sealos/flint/commit/cf2098de9185880930699e2b3c6000e02607f77b)), closes [#253](https://github.com/Sealos/flint/issues/253)
* **core:** read the kubelet's refusal only when launching an exec, from a plain 500 ([#289](https://github.com/Sealos/flint/issues/289)) ([31b0346](https://github.com/Sealos/flint/commit/31b034679e01e2c05186a6fcf3255d010d99dd64)), closes [#262](https://github.com/Sealos/flint/issues/262)
* **core:** show the kubelet's reason when a shell can't open in a stopped container ([#287](https://github.com/Sealos/flint/issues/287)) ([c70b7ad](https://github.com/Sealos/flint/commit/c70b7add4300dcc235b3d952333a5862b4862d6c)), closes [#262](https://github.com/Sealos/flint/issues/262)
* **logs:** plan again when a log tab closes ([#302](https://github.com/Sealos/flint/issues/302)) ([99da87a](https://github.com/Sealos/flint/commit/99da87a6a30c7140a87f489da260dec2a248aa35))
* **mcp:** agent activity log follow-ups ([#304](https://github.com/Sealos/flint/issues/304)) ([f147af3](https://github.com/Sealos/flint/commit/f147af36cf2c50516c2fc285b50c5189067338c2))


### Performance

* **core:** bound the core event queue ([#299](https://github.com/Sealos/flint/issues/299)) ([37c3f1f](https://github.com/Sealos/flint/commit/37c3f1f831c3a6b3b5c5003a10c0035c149f70f5))
* **core:** budget queued exec input bytes ([#294](https://github.com/Sealos/flint/issues/294)) ([904fbe3](https://github.com/Sealos/flint/commit/904fbe3ed515ab667fed889e7e67e4a997749061))
* **core:** keep a relist's strings without rehashing every row ([#300](https://github.com/Sealos/flint/issues/300)) ([0082f90](https://github.com/Sealos/flint/commit/0082f904004799cff31be1e671816ffd243221a3)), closes [#295](https://github.com/Sealos/flint/issues/295)
* **core:** shrink the interner after small scale-downs, and drop superseded rows' strings ([#305](https://github.com/Sealos/flint/issues/305)) ([ba3699e](https://github.com/Sealos/flint/commit/ba3699ed8e8c9531ca270765fa4cafc7edc5e79a))
* **logs:** share one memory budget across open log tabs ([#285](https://github.com/Sealos/flint/issues/285)) ([ef7ae9f](https://github.com/Sealos/flint/commit/ef7ae9f07ecfac0b931434fab6a1d6e1ae36ca74)), closes [#254](https://github.com/Sealos/flint/issues/254)
* **mcp:** bound in-flight tool calls and queued replies ([#292](https://github.com/Sealos/flint/issues/292)) ([b597113](https://github.com/Sealos/flint/commit/b597113e63d0e17f0b18dab1c7083051dead6474))

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
