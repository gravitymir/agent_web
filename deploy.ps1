# Build the release binary and deploy it (with static/ and .env) to the prod
# folder. Chats live next to the exe there (agent_web_prod\chats) — outside
# target\, so `cargo clean` never wipes them. Run from the repo root:
#     powershell -ExecutionPolicy Bypass -File deploy.ps1
$ErrorActionPreference = "Stop"

$dst     = "C:\Users\gravi\Documents\agent_web_prod"
$exeName = "agent_web.exe"

Write-Host "Building release..."
cargo build --release
if ($LASTEXITCODE -ne 0) { Write-Host "[X] build failed"; exit 1 }

# Stop a running prod instance so its exe isn't locked during copy.
Get-Process agent_web -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -eq (Join-Path $dst $exeName) } |
    ForEach-Object { Write-Host "Stopping running prod instance (PID $($_.Id))"; Stop-Process -Id $_.Id -Force }
Start-Sleep -Milliseconds 500

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
