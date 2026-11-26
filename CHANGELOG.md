# Changelog

## [0.7.0](https://github.com/Sealos/flint/compare/v0.6.0...v0.7.0) (2026-11-26)


### Features

* **app:** add a disconnect action for a cluster ([#667](https://github.com/Sealos/flint/issues/667)) ([5dcdb46](https://github.com/Sealos/flint/commit/5dcdb4602883b80f8a0f99f3e92db88a519a44e3))
* **app:** add a Settings view with a save and revert draft ([#745](https://github.com/Sealos/flint/issues/745)) ([5ad1c29](https://github.com/Sealos/flint/commit/5ad1c290b5c714d2942f8c91b74c402cdc0bdad8))
* **app:** add CPU and memory columns for workload lists ([#822](https://github.com/Sealos/flint/issues/822)) ([2b1a3ca](https://github.com/Sealos/flint/commit/2b1a3ca117fb4956aafa734339dcd5e991dbc301))
* **app:** add File, Cluster and Resource menus ([#808](https://github.com/Sealos/flint/issues/808)) ([449edd4](https://github.com/Sealos/flint/commit/449edd4c57e8257fac7f092eb6fc82601cda4bc8))
* **app:** add go forward, recent objects in the switcher and a wrapping palette footer ([#853](https://github.com/Sealos/flint/issues/853)) ([54ec368](https://github.com/Sealos/flint/commit/54ec368600a3e10922556ac155d222829964b20f))
* **app:** apply a proxy from Settings with hot reconnect ([#856](https://github.com/Sealos/flint/issues/856)) ([a5a92b3](https://github.com/Sealos/flint/commit/a5a92b36f38b4f609f0defefe53639b946721184))
* **app:** apply YAML and delete across a cluster set ([#750](https://github.com/Sealos/flint/issues/750)) ([1119477](https://github.com/Sealos/flint/commit/11194775b790b12a81a9f9fa2a2e6d2d5ca79f25))
* **app:** configurable metrics history length, default 6 h, with per-cluster override ([#820](https://github.com/Sealos/flint/issues/820)) ([d37fee5](https://github.com/Sealos/flint/commit/d37fee5852c56b81b37db5f8a5a59e017ceecd1c))
* **app:** edit Secrets without writing masked values back ([#749](https://github.com/Sealos/flint/issues/749)) ([18fd920](https://github.com/Sealos/flint/commit/18fd92005543627271eb9a4add3c2b1bc50f24cd))
* **app:** edit status on kinds without a status subresource ([#791](https://github.com/Sealos/flint/issues/791)) ([e08d640](https://github.com/Sealos/flint/commit/e08d6400c4a2cb65e751f49978c423603eb35149))
* **app:** fleet home screen on launch ([#830](https://github.com/Sealos/flint/issues/830)) ([a429483](https://github.com/Sealos/flint/commit/a4294837cdf62f20768e9bc4f445600083683d4e))
* **app:** keep the highlighted entry of a row menu when a live update rebuilds it ([#817](https://github.com/Sealos/flint/issues/817)) ([e50594c](https://github.com/Sealos/flint/commit/e50594ca1fb0f20b28bc5fd6c4a7e33998ec87bb))
* **app:** log buffer, path and local-protection settings in Settings ([#783](https://github.com/Sealos/flint/issues/783)) ([dffe4e1](https://github.com/Sealos/flint/commit/dffe4e17dcbd4d34e74f7f64d81e87876ccf2d09))
* **app:** min-max band sparkline with hover readout on metric tiles ([#803](https://github.com/Sealos/flint/issues/803)) ([d3e8b07](https://github.com/Sealos/flint/commit/d3e8b07db05751f008435fa86cc169444dfb7990))
* **app:** node request tick and workload rings in the detail pane ([#824](https://github.com/Sealos/flint/issues/824)) ([76010bf](https://github.com/Sealos/flint/commit/76010bfe98c2e6909335e16c1972ffa16bd2751f))
* **app:** open settings as a modal over the main window ([#859](https://github.com/Sealos/flint/issues/859)) ([85f2ae2](https://github.com/Sealos/flint/commit/85f2ae2ced69f3a8d315b3fd8ce10f2bb00505b8))
* **app:** rebuild the menu bar only when its model changed ([#839](https://github.com/Sealos/flint/issues/839)) ([330ec86](https://github.com/Sealos/flint/commit/330ec868e3cb2e0c0bbedcec9f0b2b3407daf892))
* **app:** run a fleet action across a cluster set's members ([#719](https://github.com/Sealos/flint/issues/719)) ([bd57cfd](https://github.com/Sealos/flint/commit/bd57cfdc66676f7f98b15e93c2f2bdf3eb557347))
* **app:** show a success notice after a write action ([#790](https://github.com/Sealos/flint/issues/790)) ([e71fb16](https://github.com/Sealos/flint/commit/e71fb1666182dba756b840c3d70c39b78d8fdb28)), closes [#770](https://github.com/Sealos/flint/issues/770)
* **app:** show deployment rollouts in the overview's recent activity ([#720](https://github.com/Sealos/flint/issues/720)) ([21fc556](https://github.com/Sealos/flint/commit/21fc5566dd68e01ad1b8abc602ea1257f830e7c7))
* **app:** show drain progress per pod and name the blocking budget ([#747](https://github.com/Sealos/flint/issues/747)) ([df5d89c](https://github.com/Sealos/flint/commit/df5d89c4b605fa94b6bf39f63dabbfa2b410fc82))
* **app:** sign a set member in once from the set view ([#826](https://github.com/Sealos/flint/issues/826)) ([60669b2](https://github.com/Sealos/flint/commit/60669b2492d7760e5cb1233a50a64f65238641e4)), closes [#819](https://github.com/Sealos/flint/issues/819)
* **app:** sign a watched cluster in once from its fleet home card ([#875](https://github.com/Sealos/flint/issues/875)) ([857d9a1](https://github.com/Sealos/flint/commit/857d9a1f2b34d9daa8576a14942595b0e210a9ee))
* **app:** start numeric column sorts descending and add direct sort actions ([#840](https://github.com/Sealos/flint/issues/840)) ([063e3a1](https://github.com/Sealos/flint/commit/063e3a1c5b90b2e23452720789cc0bc448e2abd9))
* **app:** threshold settings and cluster override table in Settings ([#784](https://github.com/Sealos/flint/issues/784)) ([3d4d572](https://github.com/Sealos/flint/commit/3d4d572ddb7f9aabe2566aff9a2d397d78a372a8))
* **app:** usage bars against request and limit in pod and node lists ([#815](https://github.com/Sealos/flint/issues/815)) ([7fee43b](https://github.com/Sealos/flint/commit/7fee43b492a1bfe8862d640d09ce483f6e67327c))
* **app:** usage rings against request and limit in the detail pane ([#807](https://github.com/Sealos/flint/issues/807)) ([e59dc9f](https://github.com/Sealos/flint/commit/e59dc9fbabc800b662a1826782cba4f9895e8b52))
* **app:** warn before quit or close window stops running port forwards ([#718](https://github.com/Sealos/flint/issues/718)) ([b23d984](https://github.com/Sealos/flint/commit/b23d984ff3834cc177aeac98f0078c3eb5d348be)), closes [#701](https://github.com/Sealos/flint/issues/701)
* **core:** apply a new proxy setting and reconnect every cluster without a restart ([#789](https://github.com/Sealos/flint/issues/789)) ([94d74be](https://github.com/Sealos/flint/commit/94d74be4fc30d87af62a7d2b9a4223eceea4b6d4))
* **core:** bind background sign-in consent to the plugin binary ([#722](https://github.com/Sealos/flint/issues/722)) ([c18789a](https://github.com/Sealos/flint/commit/c18789a918e41b940c26a18e7b590e04c3b1dfa9))
* **core:** connect through socks5 and socks5h proxies ([#727](https://github.com/Sealos/flint/issues/727)) ([dc92a1e](https://github.com/Sealos/flint/commit/dc92a1e700969ffd2dae9c9d52b6f83d14b82c46))
* **core:** count a pod as a problem after five minutes Pending ([#738](https://github.com/Sealos/flint/issues/738)) ([5f0b478](https://github.com/Sealos/flint/commit/5f0b478e25d9f9bf85152f1f303da9053b2f5cc8))
* **core:** fleet actions wait for each cluster's rollout before the next ([#874](https://github.com/Sealos/flint/issues/874)) ([2e5f3e4](https://github.com/Sealos/flint/commit/2e5f3e4753575c60ac62376e3bd10dbc698a0e22))
* **core:** hand a kept cluster's warm feed over when it becomes active ([#709](https://github.com/Sealos/flint/issues/709)) ([aa9f9ee](https://github.com/Sealos/flint/commit/aa9f9ee86108747b676294df32fb2b3ece664440))
* **core:** proxy setting in settings.toml for cluster connections ([#708](https://github.com/Sealos/flint/issues/708)) ([544c818](https://github.com/Sealos/flint/commit/544c81845e8a8dc27bb2707e05d5635affa10fd2))
* **core:** read pod requests, limits, node and owner for the lists ([#809](https://github.com/Sealos/flint/issues/809)) ([72e5c1a](https://github.com/Sealos/flint/commit/72e5c1adf79bb53b52dc528ac8a48c9da2d5bba9))
* **core:** run helm on plugin sign-in clusters under background consent ([#710](https://github.com/Sealos/flint/issues/710)) ([b3f9c73](https://github.com/Sealos/flint/commit/b3f9c73f20fa9f0422e50989a3bd3b1325fe1a5a))
* **core:** scale custom resources that declare a scale subresource ([#748](https://github.com/Sealos/flint/issues/748)) ([50bc231](https://github.com/Sealos/flint/commit/50bc2310e42e15be258e4c7dbf9787fe16f664bf))
* **core:** warm lists across switches and problem detection for On-view clusters ([#732](https://github.com/Sealos/flint/issues/732)) ([f5cd9aa](https://github.com/Sealos/flint/commit/f5cd9aa4a1c8ebd3095f9f68ad50564b5b0a7f70)), closes [#702](https://github.com/Sealos/flint/issues/702) [#715](https://github.com/Sealos/flint/issues/715) [#731](https://github.com/Sealos/flint/issues/731)


### Bug Fixes

* **app:** ask once each when leaving an unsaved set, and drag from set and held-edit tops ([#843](https://github.com/Sealos/flint/issues/843)) ([fe75b93](https://github.com/Sealos/flint/commit/fe75b93ff275b91f7b14d276e881b7e4c875425e))
* **app:** bind a YAML apply to the connection its review started on ([#686](https://github.com/Sealos/flint/issues/686)) ([32d8182](https://github.com/Sealos/flint/commit/32d81825dbb353f050de6cda959074e104dbbb5d))
* **app:** cluster set UI fixes: leaving, saving, namespace filter, saved list ([#788](https://github.com/Sealos/flint/issues/788)) ([19f0011](https://github.com/Sealos/flint/commit/19f00118529eca715d9b129dbe8ee80de2d11ff8))
* **app:** correct Settings copy and ask about an unapplied proxy at quit ([#869](https://github.com/Sealos/flint/issues/869)) ([3b9876b](https://github.com/Sealos/flint/commit/3b9876bcc5426e1a1ce78ec2177dcd4223ae2b9f))
* **app:** delay tooltips, cap their width and shorten kubeconfig paths ([#802](https://github.com/Sealos/flint/issues/802)) ([296061b](https://github.com/Sealos/flint/commit/296061b639f17022282d72c7ce5557d46b13f87c))
* **app:** drop the removed not-watched card state from the set dashboard state ([#764](https://github.com/Sealos/flint/issues/764)) ([385e598](https://github.com/Sealos/flint/commit/385e59813aa94b03f98ae857c49a9659acf0e7ca))
* **app:** drop usage columns before age and restarts in a narrow list ([#870](https://github.com/Sealos/flint/issues/870)) ([11c848d](https://github.com/Sealos/flint/commit/11c848d2b0e0cba8d96ab374c73826dd9b73e065))
* **app:** give a cluster set a header and keep header controls in place ([#811](https://github.com/Sealos/flint/issues/811)) ([2e1a29e](https://github.com/Sealos/flint/commit/2e1a29e37f7588bd6048d14b4b6f420f21ee7429))
* **app:** keep combined list column widths steady when a set view opens ([#864](https://github.com/Sealos/flint/issues/864)) ([bc6c098](https://github.com/Sealos/flint/commit/bc6c098dc23bb7bbb2d4a3a31534d40014c405d0))
* **app:** keep list clicks reliable and say why a click was refused ([#787](https://github.com/Sealos/flint/issues/787)) ([40f0bf3](https://github.com/Sealos/flint/commit/40f0bf3b255bf43f9bd9c2b5aca194969838e3a5))
* **app:** keep list column widths steady when a view opens ([#860](https://github.com/Sealos/flint/issues/860)) ([ced4efd](https://github.com/Sealos/flint/commit/ced4efdbd3f085d5cec5d16b2712da87385a535e))
* **app:** keep names and addresses readable beside the detail pane and in narrow windows ([#816](https://github.com/Sealos/flint/issues/816)) ([9ec2133](https://github.com/Sealos/flint/commit/9ec2133f5e97ad8dead59bed0af4064c3aa5bc4a))
* **app:** keep the overview still after the check and centre dialogs ([#786](https://github.com/Sealos/flint/issues/786)) ([46c52e1](https://github.com/Sealos/flint/commit/46c52e18e271bf70040431be3e650273fd993daf))
* **app:** keep usage columns in narrow lists and fix four macOS app e2e tests ([#868](https://github.com/Sealos/flint/issues/868)) ([1672327](https://github.com/Sealos/flint/commit/1672327afca0b0601b1f9fef1ce051b04b722060))
* **app:** keep window drag and resize handles off controls and content ([#827](https://github.com/Sealos/flint/issues/827)) ([5ef08e9](https://github.com/Sealos/flint/commit/5ef08e99a690a81a3b51bdd45c9faa06044e9b1c))
* **app:** list the selected object's actions first and match palette searches by word ([#793](https://github.com/Sealos/flint/issues/793)) ([f7397f4](https://github.com/Sealos/flint/commit/f7397f4977ee44bcf360f685e537ac4e647c858e))
* **app:** mark open-set clusters in the sidebar and combine Cmd-click with the active cluster ([#804](https://github.com/Sealos/flint/issues/804)) ([e7b14a1](https://github.com/Sealos/flint/commit/e7b14a162616e51d2e46a1aa89805e4dbfa233cf)), closes [#768](https://github.com/Sealos/flint/issues/768) [#761](https://github.com/Sealos/flint/issues/761)
* **app:** navigation history follow-ups ([#865](https://github.com/Sealos/flint/issues/865)) ([c1e8731](https://github.com/Sealos/flint/commit/c1e8731020cdb72bbe31b0b2c6afbdcf4b0d4004))
* **app:** one quiet place for signed-out members in a cluster set ([#823](https://github.com/Sealos/flint/issues/823)) ([0ca921f](https://github.com/Sealos/flint/commit/0ca921ff849aea79409e0579847cd2d946156086))
* **app:** one usage tone map, one amount format, keyboard-reachable card links ([#867](https://github.com/Sealos/flint/issues/867)) ([0b83913](https://github.com/Sealos/flint/commit/0b8391308c1141476a27c6defbf0de9eaedfd66b))
* **app:** place navigation tooltips beside their row ([#855](https://github.com/Sealos/flint/issues/855)) ([e45d6db](https://github.com/Sealos/flint/commit/e45d6db38329949c342e7c22c83e097eaf8b3ebb)), closes [#776](https://github.com/Sealos/flint/issues/776)
* **app:** show a chevron on detail buttons that open a container menu ([#825](https://github.com/Sealos/flint/issues/825)) ([029b240](https://github.com/Sealos/flint/commit/029b240498e44ffcd7c44898d76029836e4a9134)), closes [#771](https://github.com/Sealos/flint/issues/771)
* **app:** show each set member's own CPU and memory, or why they are missing ([#792](https://github.com/Sealos/flint/issues/792)) ([e193c31](https://github.com/Sealos/flint/commit/e193c3178ee1f93940ee892bf23be35bec4ea065))
* **app:** show Shell disabled on paused set rows ([#724](https://github.com/Sealos/flint/issues/724)) ([b8fda92](https://github.com/Sealos/flint/commit/b8fda92c73dcd761768875da9c3269ee96f7c202))
* **core:** ask again before turning protection off after any settings file edit ([#862](https://github.com/Sealos/flint/issues/862)) ([0f9f74e](https://github.com/Sealos/flint/commit/0f9f74ed31652dd40c29dc111b9293deb0de5e42))
* **core:** bind detail pane access checks to the slot and definition shown ([#736](https://github.com/Sealos/flint/issues/736)) ([0c1ad1e](https://github.com/Sealos/flint/commit/0c1ad1e6b06314216a0f593d067555b2e84a9239))
* **core:** keep a watched cluster's CPU and memory history across feed hand-offs ([#810](https://github.com/Sealos/flint/issues/810)) ([3ff92bc](https://github.com/Sealos/flint/commit/3ff92bcbfc7f09701b76451bf445612e3be0f65e))
* **core:** make every queue into the core unbounded by type ([#733](https://github.com/Sealos/flint/issues/733)) ([4fb19cd](https://github.com/Sealos/flint/commit/4fb19cd3416af820cccd736780e966ab1c296dad))
* **core:** proxy follow-ups after SOCKS5 support ([#734](https://github.com/Sealos/flint/issues/734)) ([bc84100](https://github.com/Sealos/flint/commit/bc84100de9482da1442e7418a192acc4566916f2)), closes [#728](https://github.com/Sealos/flint/issues/728)
* **core:** refuse a proxy apply over a saved proxy changed on disk ([#863](https://github.com/Sealos/flint/issues/863)) ([38ff416](https://github.com/Sealos/flint/commit/38ff416b8ab8d62146f3d57358e6e33a4a448273)), closes [#857](https://github.com/Sealos/flint/issues/857)
* **core:** reload the activity log when its key file changes ([#850](https://github.com/Sealos/flint/issues/850)) ([f528d77](https://github.com/Sealos/flint/commit/f528d77c3bd153b0b0fc266a88d7b42ac376e5b9))
* **core:** show why a helm release failed and hold its plugin like keep watching ([#725](https://github.com/Sealos/flint/issues/725)) ([8b907a4](https://github.com/Sealos/flint/commit/8b907a494afbdb424b2e8dc526b3ba8dddc6e6eb))
* **deps:** bump kube to 5.2.0 ([#852](https://github.com/Sealos/flint/issues/852)) ([d0a6cf7](https://github.com/Sealos/flint/commit/d0a6cf76272b94b7633b5c355b857a23bdf0251c))
* helm view proxy, pending releases and entry point ([#689](https://github.com/Sealos/flint/issues/689)) ([4e21b62](https://github.com/Sealos/flint/commit/4e21b62b327fb2443b8fa1bacc173a064379b6d7))
* v0.6 review leftovers (port-forward half-close, wording, telemetry churn) ([#693](https://github.com/Sealos/flint/issues/693)) ([c8174f5](https://github.com/Sealos/flint/commit/c8174f56e4f905e01d1936e9ee21c353f90aa3b3))


### Performance

* **app:** add up workload usage totals off the UI thread ([#861](https://github.com/Sealos/flint/issues/861)) ([b739742](https://github.com/Sealos/flint/commit/b739742cb7b04f599235baf651c940de24ab118b)), closes [#836](https://github.com/Sealos/flint/issues/836)
* **core:** skip unchanged discovery on kept clusters and back off ([#690](https://github.com/Sealos/flint/issues/690)) ([0e07680](https://github.com/Sealos/flint/commit/0e0768041d713b05d44fb33ad314132b2af965d8))

## [0.6.0](https://github.com/Sealos/flint/compare/v0.5.0...v0.6.0) (2026-11-24)


### Features

* **app:** add a dashboard for a cluster set ([#662](https://github.com/Sealos/flint/issues/662)) ([c92559d](https://github.com/Sealos/flint/commit/c92559d22ecb4bb4bd26ac92601ac6556f2a9d1c))
* **app:** add a problems only filter to lists ([#672](https://github.com/Sealos/flint/issues/672)) ([4188999](https://github.com/Sealos/flint/commit/4188999769fe69fa9c2c2ef292a0426bce6147fe))
* **app:** add standard undo, window, services and help menu items ([#534](https://github.com/Sealos/flint/issues/534)) ([2f66096](https://github.com/Sealos/flint/commit/2f6609669ce8ab068383b8eb2f5217c312d49d74))
* **app:** add the port-forward panel ([#671](https://github.com/Sealos/flint/issues/671)) ([e942aed](https://github.com/Sealos/flint/commit/e942aedc308e5ce247e6e33d8e18a5db5add0e4c))
* **app:** ask before discarding an unapplied YAML edit ([#658](https://github.com/Sealos/flint/issues/658)) ([b497c4b](https://github.com/Sealos/flint/commit/b497c4b78e738440ffd1b4bad268cdba124dec60))
* **app:** blend the macos title bar into the window ([#622](https://github.com/Sealos/flint/issues/622)) ([0ed444a](https://github.com/Sealos/flint/commit/0ed444ac4d64ec52f483cc9d5be291cf86d26686))
* **app:** disable actions the cluster's RBAC forbids ([#655](https://github.com/Sealos/flint/issues/655)) ([e3e0a11](https://github.com/Sealos/flint/commit/e3e0a1143024f8b3bbdf5ec0dcffb62fc55ceac0))
* **app:** keep watching clusters in the background ([#653](https://github.com/Sealos/flint/issues/653)) ([657d574](https://github.com/Sealos/flint/commit/657d574afb872fe8481e4fbbb8acd2487be86af7)), closes [#101](https://github.com/Sealos/flint/issues/101)
* **app:** make access mode and scope clear in the header ([#614](https://github.com/Sealos/flint/issues/614)) ([52d807a](https://github.com/Sealos/flint/commit/52d807a99a3c122cb6e10e73b13f9fe5e5820139))
* **app:** notify about problems across clusters ([#645](https://github.com/Sealos/flint/issues/645)) ([2172951](https://github.com/Sealos/flint/commit/2172951fdc0251ea014a744023e4d6e104e55898))
* **app:** offer background sign-in on a set member that needs it ([#681](https://github.com/Sealos/flint/issues/681)) ([505356f](https://github.com/Sealos/flint/commit/505356fe6ea804511f634f5d2e9e77ebdec5b15c))
* **app:** open a cluster on its overview ([#661](https://github.com/Sealos/flint/issues/661)) ([fc17700](https://github.com/Sealos/flint/commit/fc177008fbdeb8c1bc6f9fc221c78a12ffb7923d))
* **app:** open and investigate a cluster from its alerts ([#648](https://github.com/Sealos/flint/issues/648)) ([b5ce828](https://github.com/Sealos/flint/commit/b5ce828d060bbcbd89689b30c8ad8a62604d5ab5)), closes [#210](https://github.com/Sealos/flint/issues/210)
* **app:** open several clusters as a set in a combined list ([#631](https://github.com/Sealos/flint/issues/631)) ([92b0f9c](https://github.com/Sealos/flint/commit/92b0f9c0492b50078384f1a645bca0c4489576da))
* **app:** open the cluster overview from the navigation ([#633](https://github.com/Sealos/flint/issues/633)) ([78d57bf](https://github.com/Sealos/flint/commit/78d57bf5986b832a0f47cfdd70d37410e66f97df))
* **app:** open the cross-cluster compare view from an object ([#644](https://github.com/Sealos/flint/issues/644)) ([185ae83](https://github.com/Sealos/flint/commit/185ae83fc86c207c635948da28d5c217d7345472)), closes [#449](https://github.com/Sealos/flint/issues/449)
* **app:** rename and delete cluster sets ([#646](https://github.com/Sealos/flint/issues/646)) ([10eba68](https://github.com/Sealos/flint/commit/10eba68f22017a5e68c9a9d68b170dad4c580205)), closes [#219](https://github.com/Sealos/flint/issues/219)
* **app:** retry a failed connection and show its progress ([#650](https://github.com/Sealos/flint/issues/650)) ([c37ffe1](https://github.com/Sealos/flint/commit/c37ffe1669de73d30270c19dbfe429cd05feef37)), closes [#425](https://github.com/Sealos/flint/issues/425)
* **app:** show CPU and memory usage in pod and node views ([#665](https://github.com/Sealos/flint/issues/665)) ([bca5271](https://github.com/Sealos/flint/commit/bca5271536bf4af54f2d1277a2602f76531d5b74))
* **app:** show every kind a cluster set serves ([#649](https://github.com/Sealos/flint/issues/649)) ([006f10f](https://github.com/Sealos/flint/commit/006f10f3ef491169b21333f89aab2240277f022f))
* **app:** show Helm releases read-only ([#670](https://github.com/Sealos/flint/issues/670)) ([e616a71](https://github.com/Sealos/flint/commit/e616a713a381f268d0b863feb79f90c382aff6df))
* **app:** show problems and capacity on the cluster overview ([#652](https://github.com/Sealos/flint/issues/652)) ([7b516ee](https://github.com/Sealos/flint/commit/7b516ee4e952090d55201c44992aa5f11d284e07)), closes [#101](https://github.com/Sealos/flint/issues/101)
* **app:** show setup steps in the empty activity view ([#602](https://github.com/Sealos/flint/issues/602)) ([a9475cb](https://github.com/Sealos/flint/commit/a9475cb4c279f10335e4d90b36c6d619a9e92d4c))
* **app:** show when writes are paused ([#625](https://github.com/Sealos/flint/issues/625)) ([8e62ac0](https://github.com/Sealos/flint/commit/8e62ac09f1c9cd415a5bcdf5ecd936fe78b35a69))
* **core:** connect to clusters through an HTTP CONNECT proxy ([#668](https://github.com/Sealos/flint/issues/668)) ([5975690](https://github.com/Sealos/flint/commit/597569027e01be5de3ad9001c5223a4bd6f118f0))
* **core:** keep watches for set members and abort superseded attempts ([#587](https://github.com/Sealos/flint/issues/587)) ([0a1c491](https://github.com/Sealos/flint/commit/0a1c491b43a7cd885bc8c55a54c63dd3bec3bf49)), closes [#445](https://github.com/Sealos/flint/issues/445)
* **logs:** add a regex toggle to log search and filtering ([#630](https://github.com/Sealos/flint/issues/630)) ([2cc786f](https://github.com/Sealos/flint/commit/2cc786fad3ad4b32bc33936984a9dc0cdc8317db))
* **logs:** finish regex search and log filtering ([#666](https://github.com/Sealos/flint/issues/666)) ([0eaeba5](https://github.com/Sealos/flint/commit/0eaeba55cf8f982f3c26e7f5d8eb829c1a4cbb98))


### Bug Fixes

* **app:** alert on kept-watching clusters before they are selected ([#660](https://github.com/Sealos/flint/issues/660)) ([7dcd08e](https://github.com/Sealos/flint/commit/7dcd08e64459a53b395ed930b2bd20390653d4f0))
* **app:** clarify cluster names, changed urls and unavailable actions ([#664](https://github.com/Sealos/flint/issues/664)) ([386d3a8](https://github.com/Sealos/flint/commit/386d3a89d6989336f3f27264b8260af70c8d7bb5))
* **app:** close a confirmation when its cluster changes ([#676](https://github.com/Sealos/flint/issues/676)) ([95cf848](https://github.com/Sealos/flint/commit/95cf848ea3cc873d4c3ed6d776758ed110b9c556))
* **app:** keep focus on the editor after a dialog closes ([#643](https://github.com/Sealos/flint/issues/643)) ([a5ad98e](https://github.com/Sealos/flint/commit/a5ad98ecff812ac9d6868ab0130102d6ae3dbc52))
* **app:** keep Previous Logs disabled when a pod's logs cannot be read ([#637](https://github.com/Sealos/flint/issues/637)) ([22bcda3](https://github.com/Sealos/flint/commit/22bcda3ce047813990027434c3d7060a8f34d43b))
* **app:** keep similar cluster names distinct everywhere ([#607](https://github.com/Sealos/flint/issues/607)) ([e7f04ef](https://github.com/Sealos/flint/commit/e7f04efa72f9a135868e1f8007b1b84ca6f882be))
* **app:** keep the last list visible while reconnecting ([#609](https://github.com/Sealos/flint/issues/609)) ([38406ac](https://github.com/Sealos/flint/commit/38406ac5fd76c27222d93877d2e4705250b9ac55))
* **app:** make broader ai access an explicit choice ([#606](https://github.com/Sealos/flint/issues/606)) ([3afb59d](https://github.com/Sealos/flint/commit/3afb59d2479589ba1bbb31beeb447eb4a28cca1b))
* **app:** open a cluster set without its unavailable members ([#659](https://github.com/Sealos/flint/issues/659)) ([4277b28](https://github.com/Sealos/flint/commit/4277b2854a254b0e1c09f624a60330c2d47be81d))
* **app:** say when the namespace button covers a limited list ([#623](https://github.com/Sealos/flint/issues/623)) ([f07bf00](https://github.com/Sealos/flint/commit/f07bf00ab18c259d77567d5e9e21abbfd621da60))
* **app:** show only the server origin in notices and dialogs ([#684](https://github.com/Sealos/flint/issues/684)) ([2f3bcec](https://github.com/Sealos/flint/commit/2f3bceca642b7a33f58b6cd523ab05dc324be596))
* **app:** show which fields a reloaded edit would change back ([#632](https://github.com/Sealos/flint/issues/632)) ([7765fad](https://github.com/Sealos/flint/commit/7765fadb17b17c07545b4137cf53f72dd3313fc0))
* **app:** stream saved logs to the file ([#674](https://github.com/Sealos/flint/issues/674)) ([dd1288f](https://github.com/Sealos/flint/commit/dd1288ff74f11e221cbcd8ad09723dd995c48075))
* **app:** tell similar cluster names apart in the remaining places ([#619](https://github.com/Sealos/flint/issues/619)) ([28edd6b](https://github.com/Sealos/flint/commit/28edd6b8acb7f7454a3b833260b208af62335bec))
* **core:** bound terminal escape payloads and combining marks ([#641](https://github.com/Sealos/flint/issues/641)) ([6465e33](https://github.com/Sealos/flint/commit/6465e333bfc4a7ac99cfa60665d31636ba0d4b4d))
* **core:** cap the lines held during a log reconnect ([#640](https://github.com/Sealos/flint/issues/640)) ([d026536](https://github.com/Sealos/flint/commit/d026536cf91228308aacf27b888f4a1e4d1da660))
* **core:** connect consented exec-plugin set members in the background ([#680](https://github.com/Sealos/flint/issues/680)) ([30e7819](https://github.com/Sealos/flint/commit/30e7819728deede489a6650b0fcc3e1d577393b2))
* **core:** follow a kubeconfig link that moves to another folder ([#597](https://github.com/Sealos/flint/issues/597)) ([792118d](https://github.com/Sealos/flint/commit/792118d34ceb5651dba93b2a361549bfb45e482c))
* **core:** keep agent error text within its cap ([#601](https://github.com/Sealos/flint/issues/601)) ([596b3cf](https://github.com/Sealos/flint/commit/596b3cfb6fa2a02112bbdccdc7c2ffbc093f5839))
* **core:** refuse unsafe names before building API URLs ([#683](https://github.com/Sealos/flint/issues/683)) ([c1ea6a1](https://github.com/Sealos/flint/commit/c1ea6a152ba8031b0706086ffc06f994ca4fb99d))
* **core:** serialize settings saves across processes ([#604](https://github.com/Sealos/flint/issues/604)) ([bf7dd74](https://github.com/Sealos/flint/commit/bf7dd7426b634a0765948e0fd5f61463e178a112))
* **mcp:** keep a context's connection stable while a kubeconfig file is unreadable ([#608](https://github.com/Sealos/flint/issues/608)) ([1b57df5](https://github.com/Sealos/flint/commit/1b57df558ac2279f69647e132700592725850416))
* **mcp:** release cached clients for removed contexts ([#642](https://github.com/Sealos/flint/issues/642)) ([82b3af7](https://github.com/Sealos/flint/commit/82b3af7bcd2fb79a980b0c59259a4003ae6e423d))
* **mcp:** wait for a confirmed cluster definition before connecting ([#624](https://github.com/Sealos/flint/issues/624)) ([0779fc6](https://github.com/Sealos/flint/commit/0779fc6a8a860ecabbac7dbf74db48a6375927c3))

## [0.5.0](https://github.com/Sealos/flint/compare/v0.4.0...v0.5.0) (2026-11-22)


### Features

* **app:** add cluster colour tags pinned to the server ([#544](https://github.com/Sealos/flint/issues/544)) ([bcca9f4](https://github.com/Sealos/flint/commit/bcca9f422a3a8c62276cf8752c1459bc9e2752ee))
* **app:** disable actions the cluster's rbac forbids ([#539](https://github.com/Sealos/flint/issues/539)) ([8130066](https://github.com/Sealos/flint/commit/8130066dd4a259fc9890cb138ff51c50f09e2cd5))
* **app:** open the selected list row with enter ([#543](https://github.com/Sealos/flint/issues/543)) ([19a936d](https://github.com/Sealos/flint/commit/19a936dbd62db4804663479d938e2ae55f91564e))
* **app:** show and filter gitops-managed objects in lists ([#520](https://github.com/Sealos/flint/issues/520)) ([4f896bc](https://github.com/Sealos/flint/commit/4f896bc2b98801b61993c6ab0a97f989b49d7143))
* **core:** add a credential policy seam that allows by default ([#552](https://github.com/Sealos/flint/issues/552)) ([6dd3faf](https://github.com/Sealos/flint/commit/6dd3fafff4d8c8ba16001a15aa6d97164c550b95)), closes [#445](https://github.com/Sealos/flint/issues/445)
* **core:** add a worst-first attention rank for problem counts ([#575](https://github.com/Sealos/flint/issues/575)) ([b9c5075](https://github.com/Sealos/flint/commit/b9c507539dc59324b70ca3a8472c58c65cb2ffbb))
* **core:** add pure connection state model and scheduler ([#551](https://github.com/Sealos/flint/issues/551)) ([7ac395f](https://github.com/Sealos/flint/commit/7ac395f2b301ddbc7362e36a32dcd43eb41a2a53))
* **core:** add typed dashboard thresholds ([#548](https://github.com/Sealos/flint/issues/548)) ([d5201ba](https://github.com/Sealos/flint/commit/d5201ba2a447056b8b901d3d2218110acd980560))
* **core:** carry the owning workload on a problem ([#563](https://github.com/Sealos/flint/issues/563)) ([5730739](https://github.com/Sealos/flint/commit/573073945add9550d7d8f85ec4f9474a0cd77fab))
* **core:** classify crash loops, oom kills, not-ready nodes and failed jobs ([#545](https://github.com/Sealos/flint/issues/545)) ([620d930](https://github.com/Sealos/flint/commit/620d93044a5b7d61fb7a1bde82e66baa2aa68e4f))
* **core:** classify image pull failures and short workloads ([#549](https://github.com/Sealos/flint/issues/549)) ([00b50b7](https://github.com/Sealos/flint/commit/00b50b772673eec6f73fa53ef9fa482d898e48a2))
* **core:** decide alerts from problem snapshots ([#550](https://github.com/Sealos/flint/issues/550)) ([fa9b2be](https://github.com/Sealos/flint/commit/fa9b2be3164766f1d1f58213b2934e7520eb9f7f))
* **core:** read pod and node metrics from metrics-server ([#537](https://github.com/Sealos/flint/issues/537)) ([c1b3ef2](https://github.com/Sealos/flint/commit/c1b3ef2c4ccf8c1c93942fc71809120354f39317))
* **core:** summarise problems for the cluster dashboard ([#562](https://github.com/Sealos/flint/issues/562)) ([7573abe](https://github.com/Sealos/flint/commit/7573abe967c0979533b6ed578fa7ae10ecf2f86f))
* **logs:** add regex search to the core matcher ([#565](https://github.com/Sealos/flint/issues/565)) ([da7ca31](https://github.com/Sealos/flint/commit/da7ca31feee4c486b4c4e2d28fc9130b606a485f))


### Bug Fixes

* **app:** give lock and ai exposure controls accessible names ([#527](https://github.com/Sealos/flint/issues/527)) ([48cdd9f](https://github.com/Sealos/flint/commit/48cdd9f35398d3c74367a6673f6f2db1f2171a5a))
* **app:** keep every target line visible in write confirmations ([#546](https://github.com/Sealos/flint/issues/546)) ([189f9fc](https://github.com/Sealos/flint/commit/189f9fc0908df4a6d02bef7ca57050e756d606fb))
* **app:** keep more of the list visible beside the dock ([#533](https://github.com/Sealos/flint/issues/533)) ([5ead177](https://github.com/Sealos/flint/commit/5ead17732d107732df63ef968aff2ecfcbb6c4f0))
* **app:** keep the cluster in the breadcrumb at narrow widths ([#525](https://github.com/Sealos/flint/issues/525)) ([eb02f8c](https://github.com/Sealos/flint/commit/eb02f8c2cac08a5ae57eba36b492a75a188c8eeb))
* **app:** keep the cluster row's reconnecting status precise ([#538](https://github.com/Sealos/flint/issues/538)) ([3e18a07](https://github.com/Sealos/flint/commit/3e18a074331a02d9def3068be5faf990bbcb04a4))
* **app:** keep the dock tab list inside the window and its highlight in view ([#529](https://github.com/Sealos/flint/issues/529)) ([3dfce49](https://github.com/Sealos/flint/commit/3dfce49957dddde1c5a236cbdaf6c87f430e226a))
* **app:** keep the row menu open when the row narrows ([#583](https://github.com/Sealos/flint/issues/583)) ([fd0c182](https://github.com/Sealos/flint/commit/fd0c1820b4f0ca8b9594c7fc7b378faf1dc5d678))
* **app:** name the target first in unlock and trust dialogs ([#556](https://github.com/Sealos/flint/issues/556)) ([8861b09](https://github.com/Sealos/flint/commit/8861b090ea3016591d222fb4a9157b3734114c8c))
* **app:** rank the resource switcher by relevance ([#531](https://github.com/Sealos/flint/issues/531)) ([05a935a](https://github.com/Sealos/flint/commit/05a935a1157edd583c3c96ffc091999d6f3907af))
* **app:** restore the list's text filter on go back ([#535](https://github.com/Sealos/flint/issues/535)) ([85e7249](https://github.com/Sealos/flint/commit/85e72495598b5ac4d2f32bc8a0d5369a1db7dddd))
* **app:** scroll a long fetch error and keep retry visible ([#594](https://github.com/Sealos/flint/issues/594)) ([72f2be8](https://github.com/Sealos/flint/commit/72f2be86d8c5fec50912e087cacca675d335534b))
* **app:** show the full namespace focus ring and keep sidebar rows readable ([#553](https://github.com/Sealos/flint/issues/553)) ([98dcb48](https://github.com/Sealos/flint/commit/98dcb48e29d836dbba2d30ce56415536c54fd9a2))
* **app:** stop calling a failed namespace forbidden in limited lists ([#542](https://github.com/Sealos/flint/issues/542)) ([4ff827a](https://github.com/Sealos/flint/commit/4ff827a10b818f42ee8b3ff0145e53d9a5027c59))
* **app:** tell the yaml editor to reopen an object after its context moved ([#536](https://github.com/Sealos/flint/issues/536)) ([4a18cec](https://github.com/Sealos/flint/commit/4a18cecb91327ca6620d50c2e63982aad3617e9e))
* **app:** wrap notices that end mid-word ([#532](https://github.com/Sealos/flint/issues/532)) ([bddf950](https://github.com/Sealos/flint/commit/bddf9506f9d445a67f1b4a57c594f7eb1220da99))
* **core:** apply the problem hold-down before alerting ([#576](https://github.com/Sealos/flint/issues/576)) ([d52e025](https://github.com/Sealos/flint/commit/d52e025bbe8c7cf73dce9b8a37eb71bf9702a24e))
* **core:** check the current kubeconfig before each write ([#541](https://github.com/Sealos/flint/issues/541)) ([d4bd0c2](https://github.com/Sealos/flint/commit/d4bd0c2b12c17c0d9c9650e68ab4c0af9ec308ce))
* **core:** keep a paused cluster from starting connections ([#559](https://github.com/Sealos/flint/issues/559)) ([68fe7bf](https://github.com/Sealos/flint/commit/68fe7bff4cc6717871e3f64f6079c2fb67b073ad))
* **core:** keep an out-of-order kubeconfig result from disturbing a running check ([#558](https://github.com/Sealos/flint/issues/558)) ([69d52a0](https://github.com/Sealos/flint/commit/69d52a0aece887f9b2884afd27cee9b44a4c4954))
* **core:** keep dependency and protocol traffic out of stderr ([#582](https://github.com/Sealos/flint/issues/582)) ([67f71b3](https://github.com/Sealos/flint/commit/67f71b35bd8dd1f591b9f233fe16f86072c2db96))
* **core:** make kubeconfig reads more consistent ([#555](https://github.com/Sealos/flint/issues/555)) ([1479200](https://github.com/Sealos/flint/commit/14792006670b1eb9b8c80b1d351d62b154af35dc))
* **core:** mark activity calls that cannot be tied to a server ([#557](https://github.com/Sealos/flint/issues/557)) ([7c76bea](https://github.com/Sealos/flint/commit/7c76beacaf89ea91726b45fb0bca3969a608b3fd))
* **core:** remove only activity key drafts a call created ([#528](https://github.com/Sealos/flint/issues/528)) ([ef30431](https://github.com/Sealos/flint/commit/ef30431384bd3352cfea8b1c09ed83af65e8bdd5))
* **core:** shorten error text after redacting it ([#596](https://github.com/Sealos/flint/issues/596)) ([805459f](https://github.com/Sealos/flint/commit/805459f52f303a73e26f926b72d33fc513460dd8))
* **core:** treat a settings file that cannot be edited as unreadable ([#592](https://github.com/Sealos/flint/issues/592)) ([7ec8c66](https://github.com/Sealos/flint/commit/7ec8c66ebc974d55f4b85ca9a213054d60524964))
* **deps:** update url to a patched release ([#570](https://github.com/Sealos/flint/issues/570)) ([2ecf434](https://github.com/Sealos/flint/commit/2ecf434b2ce05b1c00bba3c2ad75f7c17b484f92))
* **mcp:** apply reply redaction to recorded tool arguments ([#574](https://github.com/Sealos/flint/issues/574)) ([f5483f4](https://github.com/Sealos/flint/commit/f5483f4d78652c5b9b4b722875a2dbaf92b42c42))

## [0.4.0](https://github.com/Sealos/flint/compare/v0.3.1...v0.4.0) (2026-11-22)


### Features

* **app:** add delete, scale, restart, cordon and drain ([#395](https://github.com/Sealos/flint/issues/395)) ([c7a730a](https://github.com/Sealos/flint/commit/c7a730a1b6c529e71450ea19652d79974cddb575))
* **app:** edit yaml with review, apply and conflict reload ([#393](https://github.com/Sealos/flint/issues/393)) ([6c096ca](https://github.com/Sealos/flint/commit/6c096caf0f8e8b053aadad40e500f6b406b266ed))


### Bug Fixes

* **app:** down from the list filter selects the first row ([#455](https://github.com/Sealos/flint/issues/455)) ([d4e9cc8](https://github.com/Sealos/flint/commit/d4e9cc800b61b35399b2be2815c32c0f2df92174))
* **app:** keep list columns still while typing in the filter ([#477](https://github.com/Sealos/flint/issues/477)) ([1dd0b35](https://github.com/Sealos/flint/commit/1dd0b35a6ce599c817b6f9f2cf0b7f42a03330fd))
* **app:** keep name and status readable in a narrow list ([#494](https://github.com/Sealos/flint/issues/494)) ([72e5fbd](https://github.com/Sealos/flint/commit/72e5fbde0abc965b78161fd4e30683a921e7b6d0))
* **app:** make the namespace picker keyboard-operable ([#504](https://github.com/Sealos/flint/issues/504)) ([a2ba73c](https://github.com/Sealos/flint/commit/a2ba73c84eeab40005fc7ddc8cd6c489fd6851a2)), closes [#422](https://github.com/Sealos/flint/issues/422)
* **app:** name the missing namespace in the empty list and offer Show All ([#486](https://github.com/Sealos/flint/issues/486)) ([74623f9](https://github.com/Sealos/flint/commit/74623f91a7799872914db38526adbc9f0a6d849e))
* **app:** never preselect quit in the command palette ([#452](https://github.com/Sealos/flint/issues/452)) ([8eaaa2c](https://github.com/Sealos/flint/commit/8eaaa2c0c468259e8a86bc24345bef25aaa28a9a))
* **app:** raise the contrast of the disconnected dot ([#483](https://github.com/Sealos/flint/issues/483)) ([952684c](https://github.com/Sealos/flint/commit/952684c5483d07aef90d608d8279c4d0da592ee5))
* **app:** rank listed namespaces above a typed partial name ([#480](https://github.com/Sealos/flint/issues/480)) ([7007897](https://github.com/Sealos/flint/commit/70078979e6a130af7d837709801a808b508787dd))
* **app:** run a write confirmation once ([#471](https://github.com/Sealos/flint/issues/471)) ([c7d8a80](https://github.com/Sealos/flint/commit/c7d8a80460d7315ba2e0384e601610c5e302da19))
* **app:** set a minimum window size and a detail-only narrow layout ([#517](https://github.com/Sealos/flint/issues/517)) ([fc359aa](https://github.com/Sealos/flint/commit/fc359aaef1a457c550df0854cfa5e9f648ce1b2f))
* **app:** show a cluster's connection trouble in the sidebar ([#472](https://github.com/Sealos/flint/issues/472)) ([1ac1d27](https://github.com/Sealos/flint/commit/1ac1d27c9f43dd579a91efe491d57a74975030e2))
* **app:** show a forbidden kind as no access at once ([#516](https://github.com/Sealos/flint/issues/516)) ([711168a](https://github.com/Sealos/flint/commit/711168a4cdd92e950a567e43f4fd4a40d1c9a33d)), closes [#411](https://github.com/Sealos/flint/issues/411)
* **app:** show cleaned server addresses in the sidebar and dialogs ([#485](https://github.com/Sealos/flint/issues/485)) ([f7438ad](https://github.com/Sealos/flint/commit/f7438ada5d0c210fe0e708c6d38ecdf747e73f11))
* **app:** show distinct, single-line cluster names in dialogs ([#524](https://github.com/Sealos/flint/issues/524)) ([dcdf1f6](https://github.com/Sealos/flint/commit/dcdf1f6c0786c8f9e7459d58335d9e5fd0843975))
* **app:** show stale detail as stale and refetch it on reconnect ([#514](https://github.com/Sealos/flint/issues/514)) ([cbf167d](https://github.com/Sealos/flint/commit/cbf167db15ee13048f24f693afe9e8f8d078f8d3))
* **app:** tell apart custom cluster names that look alike ([#470](https://github.com/Sealos/flint/issues/470)) ([53c544c](https://github.com/Sealos/flint/commit/53c544c33427e78d08871ec7a6824adb202362ec))
* **core:** bind a reviewed write to its credential ([#492](https://github.com/Sealos/flint/issues/492)) ([f8a7e56](https://github.com/Sealos/flint/commit/f8a7e56173d64d6935d9c99037126e7bafedbadf))
* **core:** bind a reviewed write to its whole connection definition ([#465](https://github.com/Sealos/flint/issues/465)) ([b678e3e](https://github.com/Sealos/flint/commit/b678e3e2372ad1738128bd9c7fa8f9132db39b56))
* **core:** count only known local tools as local clusters ([#500](https://github.com/Sealos/flint/issues/500)) ([5faaad9](https://github.com/Sealos/flint/commit/5faaad99ca83d23813f408a97814649ccbc76010))
* **core:** filter the log open failure and flush once at quit ([#401](https://github.com/Sealos/flint/issues/401)) ([e43ced1](https://github.com/Sealos/flint/commit/e43ced15bc04f43df43f1f02055cf23290b4feca))
* **core:** free the api error test server responses ([#513](https://github.com/Sealos/flint/issues/513)) ([fe26714](https://github.com/Sealos/flint/commit/fe2671487a262381a6f8d0f527b07972e4f7ddb6))
* **core:** handle escaped and single quotes in error summaries ([#521](https://github.com/Sealos/flint/issues/521)) ([12be899](https://github.com/Sealos/flint/commit/12be899ae858a4ad94e3374081f5d294dfbfef80))
* **core:** hold protection changes until a kubeconfig is readable ([#515](https://github.com/Sealos/flint/issues/515)) ([e276c5f](https://github.com/Sealos/flint/commit/e276c5f7dcf41d0daaa09283a8a313e950c415e9))
* **core:** keep clusters listed during a partial kubeconfig write ([#508](https://github.com/Sealos/flint/issues/508)) ([91f60a7](https://github.com/Sealos/flint/commit/91f60a7041b89b03839b0820ab60890dabae8b4b))
* **core:** keep sync error text short and clean ([#491](https://github.com/Sealos/flint/issues/491)) ([9a793d3](https://github.com/Sealos/flint/commit/9a793d39f238f4b4943804b448da6be5b892cf3b))
* **core:** more complete gitops detection for write warnings ([#509](https://github.com/Sealos/flint/issues/509)) ([d60e9b1](https://github.com/Sealos/flint/commit/d60e9b1165a3c0cdd0aace6d227d45d82c0b60bb)), closes [#211](https://github.com/Sealos/flint/issues/211)
* **core:** publish a resynced list and its live state together ([#481](https://github.com/Sealos/flint/issues/481)) ([543b2de](https://github.com/Sealos/flint/commit/543b2de4d7103bbc69fd99199cc0a254c961ba18))
* **core:** report routine relists and retried refusals precisely ([#497](https://github.com/Sealos/flint/issues/497)) ([80af9ff](https://github.com/Sealos/flint/commit/80af9ff68f996e78188d46707dd012577f399d73))
* **core:** tidy error summaries and report pods when a drain stops early ([#507](https://github.com/Sealos/flint/issues/507)) ([1ffb559](https://github.com/Sealos/flint/commit/1ffb559cc74e38a61162c2ab8af17883449b8cca))
* **core:** tidy write error text and recheck the gate during a drain ([#482](https://github.com/Sealos/flint/issues/482)) ([8c84d4d](https://github.com/Sealos/flint/commit/8c84d4dcf74670c5db3bcc8ef32b346fa921b516))
* **core:** track the cluster entry's file and say why a pin was refused ([#398](https://github.com/Sealos/flint/issues/398)) ([4144066](https://github.com/Sealos/flint/commit/4144066d56c53875b40c3a334996ee69e156fa71))
* **core:** word a stale launch pin without claiming the server moved ([#428](https://github.com/Sealos/flint/issues/428)) ([2dffbd3](https://github.com/Sealos/flint/commit/2dffbd38b7cd32705f54ec3b13ac78b209eab920))
* **e2e:** ignore unrelated discovery requests in the marker conflict test ([#518](https://github.com/Sealos/flint/issues/518)) ([6085402](https://github.com/Sealos/flint/commit/60854022c53f02b7e7be91e5531a9972cd247f74))
* **logs:** keep a separate backoff and full tooltip for waiting tabs ([#501](https://github.com/Sealos/flint/issues/501)) ([cd853ce](https://github.com/Sealos/flint/commit/cd853ce2a478ce7fb7fd9483c49950f84034ca61))
* **logs:** no stale rows after a trim, and one publish per trim ([#403](https://github.com/Sealos/flint/issues/403)) ([c9d51c2](https://github.com/Sealos/flint/commit/c9d51c2e8c25a15d14e4075319360b7ce0de0661))
* **logs:** say why a pod's logs have not started ([#453](https://github.com/Sealos/flint/issues/453)) ([97b6d71](https://github.com/Sealos/flint/commit/97b6d718afc745d527904d846d8ef6f2f7773868))
* **mcp:** apply masking across whole responses and log tails ([#484](https://github.com/Sealos/flint/issues/484)) ([fa24546](https://github.com/Sealos/flint/commit/fa245461a3f543180581f9e8af078e8cd2793310))
* **mcp:** apply output filtering to every cluster field ([#522](https://github.com/Sealos/flint/issues/522)) ([2dd8c40](https://github.com/Sealos/flint/commit/2dd8c405b5659b29d4dc939f5950f6c01183852f))
* **mcp:** say when a scope check could not cover everything ([#402](https://github.com/Sealos/flint/issues/402)) ([39744fa](https://github.com/Sealos/flint/commit/39744fa89549b86e91185022f8508936971b2e5b))

## [0.3.1](https://github.com/Sealos/flint/compare/v0.3.0...v0.3.1) (2026-11-21)

### Upgrade note

An existing `ai_exposed_clusters` scope that names a kind by an alias (`pod`, `Deployment`, `deploy`) used to grant nothing for that name. After this upgrade it grants that kind to AI agents. Review hand-written `kinds` lists before upgrading, and remove any name agents should not read.


### Features

* **app:** custom cluster names ([#358](https://github.com/Sealos/flint/issues/358)) ([22e361f](https://github.com/Sealos/flint/commit/22e361fae18ffb7556b816f26483a268327a3f69))
* **app:** mark objects managed by argo cd or flux ([#368](https://github.com/Sealos/flint/issues/368)) ([3e80c25](https://github.com/Sealos/flint/commit/3e80c2514f72afe9fdb0ee4443e2fcac4f6380d5))
* **core:** add the write module and shared confirmation ([#380](https://github.com/Sealos/flint/issues/380)) ([8b86f09](https://github.com/Sealos/flint/commit/8b86f09d6d462d51fd6c051f654e6a8d779673dc))
* **core:** make the write gate the only way to a client that can write ([#369](https://github.com/Sealos/flint/issues/369)) ([662a672](https://github.com/Sealos/flint/commit/662a6720ec57e5e9179fddc402ed9a21dd225dcc))


### Bug Fixes

* **app:** keep a cluster's trust state consistent while settings update ([#382](https://github.com/Sealos/flint/issues/382)) ([510cb7c](https://github.com/Sealos/flint/commit/510cb7cb1b07118e67922416d906211c8912e0f1))
* **app:** keep context menus inside narrow and short windows ([#392](https://github.com/Sealos/flint/issues/392)) ([5a8ee97](https://github.com/Sealos/flint/commit/5a8ee9710dccf36f6c2a32eca842e26712f71013))
* **app:** keep long names inside the row context menu ([#339](https://github.com/Sealos/flint/issues/339)) ([5c11ad9](https://github.com/Sealos/flint/commit/5c11ad9131ae001325f42a077793ffe37e651b48))
* **app:** keep masked secret columns out of filter, sort and width ([#340](https://github.com/Sealos/flint/issues/340)) ([d20c0cb](https://github.com/Sealos/flint/commit/d20c0cb5b88e5e502d84c84f8f7ef751135e4ffe))
* **app:** name the appimage in flint mcp --help ([#357](https://github.com/Sealos/flint/issues/357)) ([b3b1afd](https://github.com/Sealos/flint/commit/b3b1afd5120994a84018d8951b6d88923998d3cd))
* **app:** report discarded and refused terminal input accurately ([#356](https://github.com/Sealos/flint/issues/356)) ([2f9da83](https://github.com/Sealos/flint/commit/2f9da8362c72f50f19909474a4b490479348f7c4))
* **core:** decide safe secret columns by role and position ([#344](https://github.com/Sealos/flint/issues/344)) ([b9191c1](https://github.com/Sealos/flint/commit/b9191c13bd2586393ac0cd0f0e81d1d26a457d7f))
* **core:** event coalescing edge cases ([#362](https://github.com/Sealos/flint/issues/362)) ([7ce3b8b](https://github.com/Sealos/flint/commit/7ce3b8ba0404d8bc4826a054adb26115ca538f76))
* **core:** harden the read-only client and client construction ([#397](https://github.com/Sealos/flint/issues/397)) ([45d8274](https://github.com/Sealos/flint/commit/45d8274542d5d04a2944afe8ecaeb703fdbe0604))
* **core:** keep a shown log tab from being trimmed and seal ended streams ([#370](https://github.com/Sealos/flint/issues/370)) ([f35fbba](https://github.com/Sealos/flint/commit/f35fbbacf963460b4fa1ab57d9b00f36ea625d5d))
* **core:** keep credentials out of an exec request's debug output ([#365](https://github.com/Sealos/flint/issues/365)) ([9f8267c](https://github.com/Sealos/flint/commit/9f8267c5b0eb6f3741e5adc87a1bedc05a3ac5df))
* **core:** open the log file off the startup path and close its gaps ([#375](https://github.com/Sealos/flint/issues/375)) ([2670cc7](https://github.com/Sealos/flint/commit/2670cc711778f640be3aa79a196252124abb4d12))
* **core:** retry only read requests ([#379](https://github.com/Sealos/flint/issues/379)) ([0f6f619](https://github.com/Sealos/flint/commit/0f6f6191854073e35ec717a1064de221924a3ae6))
* **core:** tell the dock which context changes are confirmed ([#390](https://github.com/Sealos/flint/issues/390)) ([258b43a](https://github.com/Sealos/flint/commit/258b43a755c756dbd80703dccd47396a68fd90f3))
* **core:** tell when input was lost to a fallback shell that exits at once ([#396](https://github.com/Sealos/flint/issues/396)) ([344f978](https://github.com/Sealos/flint/commit/344f9787ffda6ac881520857ee9e17203ca5c4b5))
* **mcp:** do not echo raw values in wrong-type argument errors ([#387](https://github.com/Sealos/flint/issues/387)) ([5191e11](https://github.com/Sealos/flint/commit/5191e11158bcd4c1fe813750b0663d2852243600))
* **mcp:** give setup advice and settings errors that work without the app ([#384](https://github.com/Sealos/flint/issues/384)) ([6a0d829](https://github.com/Sealos/flint/commit/6a0d829ea29f1cd39fbd756f22ba0d0e98f0d47b))
* **mcp:** record no unvalidated arguments for calls refused before admission ([#385](https://github.com/Sealos/flint/issues/385)) ([387350e](https://github.com/Sealos/flint/commit/387350ec5dd47c7203902fd8ebfa16f6d31225f6))
* **mcp:** redact file paths in kubeconfig errors ([#386](https://github.com/Sealos/flint/issues/386)) ([c0d6321](https://github.com/Sealos/flint/commit/c0d6321cab32647681c859f80e94b98f509cd00c))
* **mcp:** reject unsupported tool arguments ([#347](https://github.com/Sealos/flint/issues/347)) ([65cd05e](https://github.com/Sealos/flint/commit/65cd05eac7c4b7475056d40f15eb69d954ca8856))
* **mcp:** release cancelled calls and always finish shutdown ([#363](https://github.com/Sealos/flint/issues/363)) ([b40b418](https://github.com/Sealos/flint/commit/b40b4181c6778e7d16debdfa29242c6217031d11))
* **mcp:** report an out-of-scope request as out of scope ([#378](https://github.com/Sealos/flint/issues/378)) ([150ba8f](https://github.com/Sealos/flint/commit/150ba8f838bf13472f5575ba28e82c39c15e95d0))
* **mcp:** resolve hand-written ai scopes and report what matches nothing ([#373](https://github.com/Sealos/flint/issues/373)) ([cb5a457](https://github.com/Sealos/flint/commit/cb5a457a37531f1d2a86fd3baba33c589cb4c3b2))
* **mcp:** say when an exposed context is missing from the loaded kubeconfig ([#360](https://github.com/Sealos/flint/issues/360)) ([a30a71a](https://github.com/Sealos/flint/commit/a30a71ae0f974730baa7cc956abe5e984c32d082))
* **mcp:** show the settings path order in flint-mcp --help ([#355](https://github.com/Sealos/flint/issues/355)) ([449ae1e](https://github.com/Sealos/flint/commit/449ae1ed4eb21d97cd6af7d8a149354e6a758645))
* **mcp:** treat a client that stops reading with a full queue as gone ([#388](https://github.com/Sealos/flint/issues/388)) ([b69e34c](https://github.com/Sealos/flint/commit/b69e34c92763998a60f79d98ea4c2ba5a3161609))


### Performance

* **core:** keep failed polls and pruned strings out of the shrink mark ([#377](https://github.com/Sealos/flint/issues/377)) ([ccaf496](https://github.com/Sealos/flint/commit/ccaf49648678111067c75c4f6c26943a5a645d87))
* **core:** shrink the interner by peak capacity and completed list sizes ([#348](https://github.com/Sealos/flint/issues/348)) ([0a24cb8](https://github.com/Sealos/flint/commit/0a24cb83baf38d43c2910a05da86f51d5e1bf907))

## [0.3.0](https://github.com/Sealos/flint/compare/v0.2.1...v0.3.0) (2026-11-20)


### Features

* **app:** resource switcher, pane keys and go back ([#244](https://github.com/Sealos/flint/issues/244)) ([dec65e1](https://github.com/Sealos/flint/commit/dec65e16bdd53487b543254427acfb52b7715c90))
* **logs:** show only matching lines, an errors-only view and context lines ([#291](https://github.com/Sealos/flint/issues/291)) ([fd7b8a6](https://github.com/Sealos/flint/commit/fd7b8a686c3052881347a813b599e717ffb1cc1e))
* **mcp:** a GPUI-free `flint-mcp` for Linux (x86_64 and aarch64), attached to each release ([#306](https://github.com/Sealos/flint/issues/306)) ([1593367](https://github.com/Sealos/flint/commit/1593367)), closes [#275](https://github.com/Sealos/flint/issues/275)
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
