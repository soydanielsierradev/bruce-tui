# Bruce installer for Windows (PowerShell).
#
#   irm https://raw.githubusercontent.com/soydanielsierradev/bruce-tui/main/install.ps1 | iex
#
# Downloads the latest release binary, installs it to
# %LOCALAPPDATA%\Programs\bruce (override with $env:BRUCE_BIN_DIR) and adds that
# folder to your user PATH.
$ErrorActionPreference = 'Stop'

$repo  = 'soydanielsierradev/bruce-tui'
$asset = 'bruce-x86_64-pc-windows-msvc.zip'
$dir   = if ($env:BRUCE_BIN_DIR) { $env:BRUCE_BIN_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\bruce' }
$url   = "https://github.com/$repo/releases/latest/download/$asset"

Write-Host "Downloading $asset ..."
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$tmp = Join-Path $env:TEMP "bruce-$([guid]::NewGuid()).zip"
try {
    Invoke-WebRequest -Uri $url -OutFile $tmp
    Expand-Archive -Path $tmp -DestinationPath $dir -Force
} finally {
    if (Test-Path $tmp) { Remove-Item $tmp -Force }
}
Write-Host "Installed bruce to $dir\bruce.exe"

# Add the install dir to the user PATH if it isn't already there.
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not $userPath) { $userPath = '' }
if (($userPath -split ';') -notcontains $dir) {
    [Environment]::SetEnvironmentVariable('Path', ($userPath.TrimEnd(';') + ";$dir"), 'User')
    Write-Host "Added $dir to your user PATH. Open a NEW terminal for it to take effect."
}

# Bruce runs the Claude CLI inside its workspace, so warn if it's missing.
if (-not (Get-Command claude -ErrorAction SilentlyContinue)) {
    Write-Host ""
    Write-Warning "Claude Code ('claude') was not found on your PATH."
    Write-Host    "         Bruce runs the Claude CLI inside its workspace; install it first:"
    Write-Host    "         https://docs.claude.com/claude-code"
}

Write-Host ""
Write-Host "Done. Open a new terminal and run 'bruce' in any project directory."
