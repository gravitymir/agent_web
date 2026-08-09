# Run the locked-down SUBSCRIPTION guest container (Windows / PowerShell).
#
#   1) docker build -f Dockerfile.guest -t agent-web:guest-sub .
#   2) make sure .env has CLAUDE_CODE_OAUTH_TOKEN (from `claude setup-token`)
#   3) ./run-guest.ps1
#   4) mint a code:  docker exec -u 10001 agent-guest /app/agent_web guest new --ttl 24h --label vasya
#   5) point a Cloudflare tunnel public hostname (e.g. guest.astechlab.dev)
#      at  http://localhost:8788  and share the magic link.
#
# The subscription token is read from .env and passed as the ONLY secret the
# container gets (no separate guest.env). Everything else that lives in .env
# (Kimi key, etc.) stays OUT of the guest container — least privilege.
#
# Lockdown: read-only root fs, all Linux capabilities dropped, no privilege
# escalation, resource caps; only /workspace + /chats are writable. Claude Code's
# own managed-settings (baked into the image) deny Bash/web/MCP and confine files
# to /workspace, so the token in the env is unreachable by the agent.
# Published to host loopback only (127.0.0.1:8788) — the tunnel connects there.

$ErrorActionPreference = "Stop"

if (-not (Test-Path .env)) {
    Write-Error "'.env' not found in $(Get-Location). Run this from the project root."
    exit 1
}

# Pull only the values the guest container needs from .env (skips commented lines).
function Get-EnvVar($name) {
    $m = Select-String -Path .env -Pattern "^\s*$name\s*=" | Select-Object -First 1
    if (-not $m) { return $null }
    return (($m.Line -split '=', 2)[1]).Trim().Trim('"')
}

$token = Get-EnvVar 'CLAUDE_CODE_OAUTH_TOKEN'
if ([string]::IsNullOrWhiteSpace($token)) {
    Write-Error "CLAUDE_CODE_OAUTH_TOKEN is not set in .env (required for the subscription guest engine). Run 'claude setup-token'."
    exit 1
}
$publicUrl = Get-EnvVar 'CWI_PUBLIC_URL'
if ([string]::IsNullOrWhiteSpace($publicUrl)) { $publicUrl = 'https://guest.astechlab.dev' }

# Guest workspace on the host (the ONLY host folder the container can touch).
New-Item -ItemType Directory -Force guest-workspace | Out-Null

# Owner usage snapshot (plan + limits) — the owner instance writes it here; we
# mount it read-only so the guest badge can show "Cloud <plan>" and the 5h/week
# gauges. Ensure the FILE exists first, else Docker would mount a directory.
$snap = Join-Path $PWD "target\release\chats\usage_snapshot.json"
$snapDir = Split-Path $snap
if (-not (Test-Path $snapDir)) { New-Item -ItemType Directory -Force $snapDir | Out-Null }
if (-not (Test-Path $snap))   { '{}' | Set-Content -NoNewline -Encoding utf8 $snap }

# Remove any previous instance (ignore "no such container" on first run).
try { docker rm -f agent-guest 2>&1 | Out-Null } catch { }

docker run -d --name agent-guest `
  --restart unless-stopped `
  --read-only `
  --cap-drop ALL `
  --cap-add NET_ADMIN `
  --cap-add SETUID `
  --cap-add SETGID `
  --security-opt no-new-privileges `
  --pids-limit 512 `
  --memory 2g `
  --cpus 2 `
  --tmpfs /tmp `
  -v agent_guest_chats:/chats `
  -v "${PWD}\guest-workspace:/workspace" `
  -v "${snap}:/owner_usage.json:ro" `
  -e "CLAUDE_CODE_OAUTH_TOKEN=$token" `
  -e "CWI_PUBLIC_URL=$publicUrl" `
  -e "CWI_USAGE_FILE=/owner_usage.json" `
  -p 127.0.0.1:8788:8787 `
  agent-web:guest-sub

Write-Host ""
Write-Host "Guest container started on http://127.0.0.1:8788"
Write-Host "Mint an access code:"
Write-Host "  docker exec -u 10001 agent-guest /app/agent_web guest new --ttl 24h --label vasya --url $publicUrl"
