# Run the locked-down SUBSCRIPTION guest container (Windows / PowerShell).
#
#   1) docker build -f Dockerfile.guest -t agent-web:guest-sub .
#   2) copy guest.env.example -> guest.env and set CLAUDE_CODE_OAUTH_TOKEN
#      (from `claude setup-token`)
#   3) ./run-guest.ps1
#   4) mint a code:  docker exec agent-guest /app/agent_web guest new --ttl 24h --label vasya
#   5) point a Cloudflare tunnel public hostname (e.g. guest.astechlab.dev)
#      at  http://localhost:8788  and share the magic link.
#
# Lockdown: read-only root fs, all Linux capabilities dropped, no privilege
# escalation, resource caps; only /workspace + /chats are writable. Claude Code's
# own managed-settings (baked into the image) deny Bash/web/MCP and confine files
# to /workspace, so the subscription token in the env is unreachable by the agent.
# Published to host loopback only (127.0.0.1:8788) — the tunnel connects there.

$ErrorActionPreference = "Stop"

# Guest workspace on the host (the ONLY host folder the container can touch).
New-Item -ItemType Directory -Force guest-workspace | Out-Null

# Remove any previous instance.
docker rm -f agent-guest 2>$null | Out-Null

docker run -d --name agent-guest `
  --read-only `
  --cap-drop ALL `
  --security-opt no-new-privileges `
  --pids-limit 512 `
  --memory 2g `
  --cpus 2 `
  --tmpfs /tmp `
  -v agent_guest_chats:/chats `
  -v "${PWD}\guest-workspace:/workspace" `
  --env-file guest.env `
  -p 127.0.0.1:8788:8787 `
  agent-web:guest-sub

Write-Host ""
Write-Host "Guest container started on http://127.0.0.1:8788"
Write-Host "Mint an access code:"
Write-Host "  docker exec agent-guest /app/agent_web guest new --ttl 24h --label vasya --url https://guest.astechlab.dev"
