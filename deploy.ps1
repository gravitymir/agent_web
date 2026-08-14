# Build the release binary and deploy it (with static/ and .env) to the prod
# folder. Chats live next to the exe there (agent_web_prod\chats) — outside
# target\, so `cargo clean` never wipes them. Run from the repo root:
#     powershell -ExecutionPolicy Bypass -File deploy.ps1
$ErrorActionPreference = "Stop"

$dst     = "C:\Users\gravi\Documents\agent_web_prod"
$exeName = "agent_web.exe"

# Stop running instances FIRST — the dev one locks target\release\agent_web.exe
# (so the build would fail) and the prod one locks the copy target.
$prodExe = Join-Path $dst $exeName
$devExe  = Join-Path (Get-Location) "target\release\$exeName"
Get-Process agent_web -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -eq $prodExe -or $_.Path -eq $devExe } |
    ForEach-Object { Write-Host "Stopping agent_web (PID $($_.Id)) at $($_.Path)"; Stop-Process -Id $_.Id -Force }
Start-Sleep -Milliseconds 500

Write-Host "Building release..."
cargo build --release
if ($LASTEXITCODE -ne 0) { Write-Host "[X] build failed"; exit 1 }

New-Item -ItemType Directory -Force -Path $dst | Out-Null

# exe
Copy-Item "target\release\$exeName" (Join-Path $dst $exeName) -Force

# static/ (mirror: remove then copy so stale files don't linger)
if (Test-Path "$dst\static") { Remove-Item "$dst\static" -Recurse -Force }
Copy-Item "static" "$dst\static" -Recurse -Force

# .env (secrets/config), if present
if (Test-Path ".env") { Copy-Item ".env" (Join-Path $dst ".env") -Force }

Write-Host ""
Write-Host "[ok] Deployed to $dst"
Write-Host "     Run it:  `"$dst\$exeName`""
Write-Host "     Chats live in $dst\chats (created on first run; log in there once)."
