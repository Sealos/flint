# E2E coverage by user path

Which tests cover each path in [`USER-PATHS.md`](./USER-PATHS.md) today, at which layer, and what is missing. The plan
that closes the gaps is in [`TESTING.md`](./TESTING.md#e2e-coverage-plan). Update a row when a test for its path lands.

Audited on master at 057b773 (after #100, #106, #94 and #118), and rechecked for M2 at 194a79c by the M2 release
review (#15). "#116" marks view tests that have since merged (aa59738). The app e2e tests are `#[ignore]`d in plain
`cargo test`; CI runs them in the `macos (flint-app)` job (#117).

**Layers:** U unit, V view (`#[gpui_kit::test]`), F core or MCP against the fake API server, K cluster on kind
(`cluster-e2e`), A app e2e (automation channel, fake API server unless noted), M MCP e2e over stdio, Man manual.
**Paths:** `app/` is `crates/flint-e2e/tests/app/`, `ui/` is `crates/flint-app/src/ui/`.

## M0 and M1

| UP | Asks for | Covered by | Gap | Plan |
|---|---|---|---|---|
| 01 | U, ui | `kubeconfig.rs::merges_files_first_wins_and_tracks_sources`, `display.rs::eks_arns_show_the_cluster_name` (U); `app/m0.rs::up_01_launch_and_see_my_clusters` (A, no requests); #116 `clusters_tests.rs` (V, tooltip) | — | — |
| 02 | U, ui | 6 tests in `kubeconfig.rs` (U); `app/m0.rs::up_02_launch_with_no_usable_kubeconfig` (A) | — | — |
| 03 | K, ui | `dev_cluster.rs::core_handle_connects_and_reads_version` (K); `e2e/core.rs::connects_and_discovers_every_recorded_kind` (F); `app/m0.rs::up_03_connect_to_a_cluster` (A, also on kind) | — | — |
| 04 | U, K, ui | `cluster.rs` timeouts and missing plugin (U); `core_handle.rs::unreachable_cluster_fails_with_a_readable_error`; `app/m0.rs::up_04_connection_fails_with_a_plain_cause_and_retries` (A, unreachable only, loose text `TODO(#98)`) | missing and hanging exec plugins in the app | #130, #127 |
| 05 | U, Man | 9 tests in `env_path.rs` (U) | — | release checklist |
| 06 | U, K | `core_handle.rs::reloads_when_kubeconfig_changes`; `app/m0.rs::up_06_kubeconfig_changes_on_disk` (A) | its half-written file lasts 30 ms, under the 300 ms debounce, so a file held half-written is untested | #131 |
| 07 | U, ui | `core_handle.rs::set_ai_exposed_writes_context_and_server` (U); `ui/workspace_tests.rs::the_expose_dialog_answers_confirm_and_cancel` (V); `app/m0.rs::up_07_expose_a_cluster_to_ai` (A); #116 `expose_tests.rs` | — | — |
| 08 | U, M, ui | `policy.rs::same_name_with_a_different_server_is_not_exposed` (U); `mcp_stdio.rs::mcp_policy_without_a_cluster` (M); `app/m0.rs::up_08_up_12_app_and_flint_mcp_agree_on_exposure` (A) | — | — |
| 09 | M | `mcp_stdio.rs::mcp_over_stdio` (M+K); `e2e/mcp.rs::up_09_agent_lists_only_exposed_clusters` (M); `mcp_subcommand.rs::flint_mcp_answers_on_stdio` | — | — |
| 10 | U, M+K | 28 tagged (U); `mcp_stdio.rs::mcp_over_stdio` (M+K); `e2e/mcp.rs::up_10_agent_inspects_a_cluster`, `up_10_agent_reads_current_data` (M) | `flint-e2e` MCP tests never run on kind | #128 |
| 11 | U, M | `policy.rs::unexposed_and_unknown_clusters_give_identical_errors` (U); `e2e/mcp.rs::up_11_…look_the_same` (M, no requests) | — | — |
| 12 | U, M | `core_handle.rs::reloads_when_settings_change`; `e2e/mcp.rs::up_12_settings_edited_by_hand_apply_on_the_next_call` (M); `app/m0.rs::up_08_up_12_…` (A) | — | — |
| 13 | ui | `ui/appearance_tests.rs::switching_appearance_swaps_the_theme_colors` (V); `app/m0.rs::up_13_light_and_dark_appearance` (A) | following macOS live | manual |
| 14–16 | U, M+K, ui | — (listed under M0, shipped in M5.5: #67, #68, #69) | not built | #137 |
| 17 | U, M | — (M4: #104) | not built | #135 |
| 20 | K, ui | `watch_cluster.rs::pods_list_then_live_update_after_scaling` (K); `e2e/core.rs::live_lists_follow_watch_events` (F); `ui/list_tests.rs` (V); `app/m1.rs::up_20_browse_pods_with_live_updates` (A) | — | — |
| 21 | U, ui | `view.rs`, `ui_state.rs` (U); `ui/list_tests.rs::list_sorts_by_header_and_filters_by_namespace_and_text` (V); `app/m1.rs::up_21_filter_and_sort` (A) | — | — |
| 22 | K | `watch_cluster.rs::crd_with_printer_columns_appears_live` (K); `e2e/core.rs::custom_resources_show_their_printer_columns` (F); `app/m1.rs::up_22_browse_a_crd` (A) | — | — |
| 23 | K, ui | `watch_cluster.rs` (K); `ui/object.rs` (U); `app/m1.rs::up_23_inspect_an_object` (A); #116 `detail_tests.rs` | — | — |
| 24 | K, ui | `watch_cluster.rs::on_view_versus_all_and_switching_clusters` (K); `app/m1.rs::up_24_switch_clusters` (A) | log and shell sessions across a switch in the app | #124 |
| 25 | U, K | `watch/tests.rs::watch_mode_transitions` (U); `watch_cluster.rs::on_view_versus_all…` (K); `app/m1.rs::up_25_change_the_watch_mode` (A) | — | — |
| 26 | K, measured | `watch_load.rs::measure_all_mode_during_scale_down` (K, skipped in CI: `FLINT_LOAD_NAMESPACE` unset); `ui/list_tests.rs::measure_frame_cost_with_10k_rows` (V) | the whole app under load | #133 |
| 27 | U, K | `watch_recovery.rs::lists_recover_after_the_api_server_restarts` (K, nightly); `e2e/core.rs::relists_after_a_410…` (F); `app/m1.rs::up_27_recover_from_a_broken_watch` (A, loose text `TODO(#98)`) | strict wording | #127 |
| 28 | U, K | `table.rs::non_table_responses_fall_back_to_name_namespace_age` (U); `app/m1.rs::up_28_an_api_without_tables` (A) | — | — |

## M1.5 and M2

| UP | Asks for | Covered by | Gap | Plan |
|---|---|---|---|---|
| 30 | ui | `ui/workspace_tests.rs::a_palette_can_list_actions_with_shortcuts` (V, registry only) | not built (#29) | #132 |
| 31 | U, ui | — (#29) | not built | #132 |
| 32 | ui | `theme.rs` contrast tests (U); `ui/appearance_tests.rs` (V); screenshots in both themes from every app e2e test | — | — |
| 33 | ui | `ui/list_tests.rs::clicking_and_arrows_select_rows` (V) | sidebar keys (not built yet), Enter, quit in the app | #132 |
| 40 | K, ui | ~40 tagged in `logs/` (U); `logs_cluster.rs::follows_a_chatty_pod…`, `picks_a_container_and_reads_the_previous_instance` (K); `e2e/core.rs::a_pod_log_streams_resumes_without_repeats_and_ends_with_its_pod` (F); `ui/log_tests.rs` (V, buffer fed directly) | **the app on a real stream** | #122 (needs #119) |
| 41 | U, K, ui | 19 in `exec.rs` (U); `exec_cluster.rs::shell_into_a_container` (K, bash and sh images); `ui/terminal_tests.rs` (V, `FakeExec`) | **the app over a real exec websocket** | #123 (kind smoke first, needs #119; CI on the fake needs #121) |
| 42 | ui | `ui/workspace_tests.rs::the_dock_toggles_and_remembers_its_height`, `logs_open_in_the_dock_and_outlive_a_cluster_switch` (V); `ui/dock_tests.rs` leak tests (V) | stream closed on tab close, height across a relaunch | #124 |
| 43 | U, K, ui | `logs/line.rs::severity_follows_the_whole_word_rules` (U); `logs_cluster.rs::follows_a_chatty_pod…` (K, asserts severity and timestamps, untagged); `ui/log_tests.rs::timestamps_and_wrap_toggle_without_reloading` (V) | "without reloading" unchecked at the HTTP level | #122 |
| 44 | ui | `ui/log_tests.rs::selecting_pauses_and_copies_the_lines_as_shown` (V) | — (view layer is right) | — |
| 45 | U, K, ui | 33 tagged in `logs/` (U); `logs_cluster.rs::follows_a_deployments_pods_as_they_scale_and_get_replaced` (K); `e2e/core.rs::a_workload_log_follows_its_pods_through_churn` (F); `ui/log_tests.rs::a_workload_tab_shows_sources_and_stream_counts` (V) | no app test with pod churn | #125 |
| 46 | U, ui | `ui/log_tests.rs::save_writes_matches_as_shown_to_the_chosen_file`, `a_narrow_toolbar_keeps_save_reachable` (V) | — (the native save panel can't be automated; view layer is right) | — |

## M3 and later

None of these features is built yet; each issue lists its tests and is written with the feature.

| UP | Asks for | Plan |
|---|---|---|
| 50–55, 92 | U, K, ui, e2e | #134 (needs #129: writes recorder, remote-looking cluster) |
| 60–68, 17 | U, K, ui, e2e, mcp | #135 |
| 70–74 | K, ui | #136 (kind; the app on kind locally) |
| 75–77, 93, 14–16 | U, ui (fake `claude`), mcp | #137 |
| 78–79, 82–85 | U, K, ui, e2e | #138 |
| 80 | Man | release checklist (clean Mac, Apple Silicon and Intel) |
| 81 | Man, CI smoke | #139 |
| 90 | U, review | `core_handle.rs::commands_never_write_kubeconfig_files` (U); every app test: #127 |
| 91 | U, ui | `app/m0.rs::up_01` and `e2e/mcp.rs::up_11` assert no requests; every app test: #127 |
| 100–101 | U, e2e | P2, with #82 |
| 110–116 | U, e2e, cluster, ui, mcp | P2, with #109 to #113 (M8) |
