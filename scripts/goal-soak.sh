#!/usr/bin/env bash
set -euo pipefail

# Long-running persistence soak. The Rust test uses synthetic local operations only.
export AGENT_GOAL_SOAK_SECONDS="${AGENT_GOAL_SOAK_SECONDS:-172800}"
export AGENT_GOAL_SOAK_INTERVAL_MS="${AGENT_GOAL_SOAK_INTERVAL_MS:-1000}"

echo "Goal persistence soak: ${AGENT_GOAL_SOAK_SECONDS}s, interval ${AGENT_GOAL_SOAK_INTERVAL_MS}ms"
echo "This does not contact an LLM, network service, or external tool."
cargo test --locked soak_durable_goal_state_for_configured_duration -- --ignored --nocapture
