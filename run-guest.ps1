# Launch (or relaunch) the GUEST SANDBOX instance, detached, on :8790.
# It runs the SAME prod exe but with sandbox+auth env: every session's tools
# execute inside the executor VM (mcp-guest over SSH), gated by a magic-link code.
# Shares the prod config dir (login + guest_tokens minted on the admin "Ссылки"
# tab), but a separate workspace so guest chats don't mix with the owner's.
# Safe to call from deploy.ps1: it clears its env vars at the end so a following
# owner launch stays clean.
$ErrorActionPreference = "Stop"
$dst = "C:\Users\gravi\Documents\agent_web_prod"
$exe = Join-Path $dst "agent_web.exe"

# Stop a running guest instance (port 8790) so its exe/port is free.
$p = (Get-NetTCPConnection -LocalPort 8790 -State Listen -ErrorAction SilentlyContinue | Select-Object -First 1).OwningProcess
if ($p) { Write-Host "Stopping running guest instance (PID $p)"; Stop-Process -Id $p -Force; Start-Sleep -Milliseconds 500 }

$env:CWI_SANDBOX = "1"        # tools run in the executor VM
$env:CWI_AUTH    = "1"        # magic-link gate
$env:CWI_ADMIN   = "0"        # not admin: no VM control / link minting here
$env:CWI_ENGINE  = "cli"      # Claude Code (subscription)
$env:CWI_NO_MENU = "1"        # no interactive wizard (detached)
$env:CWI_BIND    = "127.0.0.1:8790"
$env:CLAUDE_CONFIG_DIR = Join-Path $dst "chats"            # shared login + guest_tokens
# Neutral, non-identifying workspace path. The guest's reasoning runs via the host
# CLI, which injects this cwd into the model's context — so keep it clear of the
# owner's username and "agent_web_prod" (the guest's real files live on the VM).
$env:CWI_WORKSPACE     = "C:\Users\Public\agent-guest"

Start-Process -FilePath $exe -WorkingDirectory $dst -WindowStyle Hidden `
    -RedirectStandardOutput (Join-Path $dst "sandbox.out.log") `
    -RedirectStandardError  (Join-Path $dst "sandbox.err.log")

# Clear so a caller (deploy.ps1) doesn't leak sandbox env into the owner launch.
"CWI_SANDBOX","CWI_AUTH","CWI_ADMIN","CWI_ENGINE","CWI_NO_MENU","CWI_BIND","CLAUDE_CONFIG_DIR","CWI_WORKSPACE" |
    ForEach-Object { Remove-Item "env:$_" -ErrorAction SilentlyContinue }

Start-Sleep -Milliseconds 1200
$ok = try { (Invoke-RestMethod -Uri "http://127.0.0.1:8790/api/health" -TimeoutSec 5).status } catch { "DOWN" }
Write-Host "Guest sandbox (:8790) -> $ok"
