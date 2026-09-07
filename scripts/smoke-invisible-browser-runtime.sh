#!/usr/bin/env bash
set -euo pipefail

echo "[p4] invisible browser runtime deterministic checks"
cargo test browser_runtime::tests::
cargo test chromium_driver::tests::background_launch_args_are_headless_and_interactive_launch_is_visible
cargo test chromium_driver::tests::configured_visibility_flags_cannot_override_runtime_mode
cargo test account_runtime::tests::concurrent_cold_lifecycle_requests_single_flight_startup

echo "P4 invisible browser runtime smoke passed"
