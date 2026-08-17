@echo off
REM Same as deploy.bat, but launches the OWNER headless — Cloud/CLI (Claude Code
REM subscription) on port 8787, with NO engine wizard. Double-click to rebuild,
REM deploy, and run without any prompts. Ctrl+C stops the app and returns here.
powershell -ExecutionPolicy Bypass -File "C:\Users\gravi\Documents\rust\agent_web\deploy.ps1" -Default
echo.
echo (app stopped)
pause
