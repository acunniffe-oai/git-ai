// Shared production trace2 test infrastructure.
#[macro_use]
#[allow(dead_code)]
mod repos;
mod test_utils;

mod cli_parser_rebase_args;
mod cold_trace2_repo;
mod config_cli_coverage;
mod config_pattern_detection;
mod daemon_runtime_memory;
mod debug_diagnostics;
mod git_alias_resolution;
mod git_cli_arg_parsing;
mod git_repository_comprehensive;
mod gix_config_tests;
mod http_proxy;
mod install_hooks_comprehensive;
mod install_hooks_otel;
mod internal_spawn_safety;
mod repository_unit;
mod sqlite_connection_policy;
mod superuser_guard;
mod tls_native_certs;
mod v2_checkpoints;
mod v2_commit_links;
mod v2_config;
mod v2_cutover;
mod v2_install;
mod v2_otel;
mod v2_store;

mod v2_adapters;
mod v2_checkpoint_ordering;
