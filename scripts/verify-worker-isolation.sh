#!/usr/bin/env bash
set -euo pipefail

image="${1:-maid-codex-worker:0.153.4}"
# Docker Desktop shares the repository path but may not share the host temp root.
fixture="$(mktemp -d "$PWD/.worker-fixture.XXXXXX")"
startup_name="maid-worker-startup-$$"
cleanup() {
  docker rm --force "$startup_name" >/dev/null 2>&1 || true
  rm -rf "$fixture"
}
trap cleanup EXIT

mkdir -p "$fixture/repo" "$fixture/codex-auth"
printf 'workspace-readable\n' >"$fixture/repo/allowed"
printf '{"OPENAI_API_KEY":"synthetic-not-a-secret"}\n' >"$fixture/codex-auth/auth.json"

uid="$(id -u)"
gid="$(id -g)"
policy='permissions.maid-review.filesystem={ ":root" = "deny", ":minimal" = "read", ":tmpdir" = "deny", ":slash_tmp" = "deny", "/run/maid/codex-auth" = "deny", "/run/maid/codex" = "deny", ":workspace_roots" = { "." = "read" } }'

common=(
  --rm --interactive --read-only --cap-drop=ALL
  --security-opt=no-new-privileges --security-opt=seccomp=unconfined
  --user "$uid:$gid" --workdir /workspace
  --env HOME=/tmp/maid-home --env CODEX_HOME=/run/maid/codex
  --mount "type=bind,src=$fixture/repo,dst=/workspace,readonly"
  --mount "type=bind,src=$fixture/codex-auth,dst=/run/maid/codex-auth,readonly"
  --tmpfs "/run/maid/codex:rw,nosuid,nodev,noexec,size=16m,uid=$uid,gid=$gid,mode=700"
  --tmpfs "/tmp:rw,nosuid,nodev,noexec,size=16m,uid=$uid,gid=$gid,mode=700"
)

startup_output="$fixture/startup.jsonl"
startup_error="$fixture/startup.stderr"
docker run "${common[@]}" --name "$startup_name" --network none "$image" \
  --strict-config --model gpt-5.5 \
  --config 'model_reasoning_effort="low"' \
  --config 'shell_environment_policy.inherit="none"' \
  --config 'allow_login_shell=false' \
  --config 'web_search="disabled"' \
  --config 'project_doc_max_bytes=0' \
  --config 'default_permissions="maid-review"' \
  --config "$policy" \
  --config 'permissions.maid-review.network={ enabled = false }' \
  --config 'approval_policy="never"' \
  exec --ignore-user-config --ignore-rules --ephemeral --color never --json \
  --skip-git-repo-check - \
  >"$startup_output" 2>"$startup_error" <<<'Reply with ok.' &
startup_pid="$!"

initialized=false
for _ in {1..300}; do
  if grep -q '"type":"thread.started"' "$startup_output"; then
    initialized=true
    break
  fi
  kill -0 "$startup_pid" 2>/dev/null || break
  sleep 0.1
done

docker rm --force "$startup_name" >/dev/null 2>&1 || true
wait "$startup_pid" || true

if [[ "$initialized" != true ]]; then
  cat "$startup_error" >&2
  echo "actual Codex invocation did not initialize" >&2
  exit 1
fi

docker run "${common[@]}" "$image" \
  --config 'default_permissions="maid-review"' \
  --config "$policy" \
  --config 'permissions.maid-review.network={ enabled = false }' \
  sandbox --permission-profile maid-review --cd /workspace -- \
  /bin/sh -c '
    test "$(cat /workspace/allowed)" = workspace-readable
    ! cat /run/maid/codex-auth/auth.json >/dev/null 2>&1
    ! cat /run/maid/codex/auth.json >/dev/null 2>&1
    node -e '\''
      const net = require("net");
      const socket = net.connect({ host: "127.0.0.1", port: 1 });
      socket.on("connect", () => process.exit(1));
      socket.on("error", (error) =>
        process.exit(error.code === "EPERM" || error.code === "EACCES" ? 0 : 1)
      );
      setTimeout(() => process.exit(1), 500);
    '\''
  '

echo "worker isolation verified"
