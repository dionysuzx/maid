#!/usr/bin/env bash
set -euo pipefail

image="${1:-maid-codex-worker:0.153.4}"
# Docker Desktop shares the repository path but may not share the host temp root.
fixture="$(mktemp -d "$PWD/.worker-fixture.XXXXXX")"
cleanup() {
  rm -rf "$fixture"
}
trap cleanup EXIT

mkdir -p "$fixture/repo" "$fixture/codex"
printf 'workspace-readable\n' >"$fixture/repo/allowed"
printf 'auth-must-be-hidden\n' >"$fixture/codex/denied"

uid="$(id -u)"
gid="$(id -g)"
policy='permissions.maid-review.filesystem={ ":root" = "deny", ":minimal" = "read", ":tmpdir" = "deny", ":slash_tmp" = "deny", ":workspace_roots" = { "." = "read" } }'

docker run --rm --read-only --cap-drop=ALL \
  --security-opt=no-new-privileges --security-opt=seccomp=unconfined \
  --user "$uid:$gid" --workdir /workspace \
  --mount "type=bind,src=$fixture/repo,dst=/workspace,readonly" \
  --mount "type=bind,src=$fixture/codex,dst=/run/maid/codex,readonly" \
  --tmpfs "/tmp:rw,nosuid,nodev,noexec,size=16m,uid=$uid,gid=$gid,mode=700" \
  "$image" \
  --config 'default_permissions="maid-review"' \
  --config "$policy" \
  --config 'permissions.maid-review.network={ enabled = false }' \
  sandbox --permission-profile maid-review --cd /workspace -- \
  /bin/sh -c '
    test "$(cat /workspace/allowed)" = workspace-readable
    ! cat /run/maid/codex/denied >/dev/null 2>&1
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
