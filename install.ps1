<#
.SYNOPSIS
    One-time setup for Winter.

.DESCRIPTION
    Builds the release binaries, installs the `winter` command into the Cargo
    bin directory, installs the Windows Terminal integration, and registers
    the per-user logon task that keeps the controller running.

    Run this once after cloning. Afterwards `winter` works from any shell, the
    prefix is available in every session, and the controller comes back by
    itself after a restart - no further action is required.

    Two things need administrator approval, and Windows asks for both once:
    `cargo install` is not one of them, but the logon task registration is.
    Answer the prompt and the rest of the script continues unattended.

    The script never edits the persisted (User/Machine) PATH: it only reloads
    it into the current process so the freshly installed binaries can be
    located, and warns when the install directory is not persisted.

.EXAMPLE
    PS> .\install.ps1
#>
[CmdletBinding()]
param(
    # Install the binaries and the Windows Terminal integration, but leave the
    # controller out of the sign-in sequence.
    [switch]$NoAutostart
)

Set-StrictMode -Version Latest
# Keep native-command exit codes observable through $LASTEXITCODE instead of
# turning them into errors, so the friendly `throw`s below keep working under
# PowerShell 7.3+ profiles that set $ErrorActionPreference = 'Stop'.
$PSNativeCommandUseErrorActionPreference = $false

$ErrorActionPreference = 'Stop'

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw 'Rust/Cargo was not found. Install it from https://rustup.rs and run this script again.'
}

$repoRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
Push-Location $repoRoot
try {
    Write-Host 'Installing winter, winterminalp (compatibility alias), and winterd (release build)...' -ForegroundColor Cyan
    cargo install --path . --bins --locked --force
    if ($LASTEXITCODE -ne 0) {
        throw "cargo install failed with exit code $LASTEXITCODE"
    }
}
finally {
    Pop-Location
}

# This shell may have started before the Cargo bin directory was persisted to
# PATH; reload the persisted User+Machine PATH into this process so the fresh
# install can be located. The persisted PATH itself is never modified.
$persistedUserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$persistedMachinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
$env:Path = (@($env:Path, $persistedUserPath, $persistedMachinePath) | Where-Object { $_ }) -join ';'

$winterCommand = Get-Command winter.exe -ErrorAction SilentlyContinue
$winterExe = if ($winterCommand) { $winterCommand.Source } else { $null }

if (-not $winterExe) {
    # PATH lookup failed; fall back to cargo's own install list plus the
    # directory that hosts the cargo executable.
    $installedBins = @(cargo install --list 2>$null) | ForEach-Object { "$_".Trim() }
    $cargoCommand = Get-Command cargo -ErrorAction SilentlyContinue
    if ($cargoCommand) {
        $winterCandidate = Join-Path (Split-Path -Parent $cargoCommand.Source) 'winter.exe'
        if (($installedBins -contains 'winter.exe') -and (Test-Path -LiteralPath $winterCandidate)) {
            $winterExe = $winterCandidate
        }
    }
}

if (-not $winterExe) {
    throw "winter.exe was not found. Ensure the Cargo bin directory is on PATH, then re-run '.\install.ps1'."
}

# Without winterd.exe, `winter launch` falls back to a foreground controller
# and the hidden background daemon silently degrades; treat that as an error.
$winterdExe = Join-Path (Split-Path -Parent $winterExe) 'winterd.exe'
if (-not (Test-Path -LiteralPath $winterdExe)) {
    throw "winterd.exe was not found next to $winterExe; the hidden background controller would silently degrade. Re-run '.\install.ps1' or 'cargo install --path . --bins --locked --force'."
}

# Warn when the install directory is missing from the persisted PATH (User or
# Machine), so new shells will still be able to run 'winter'.
$winterDir = (Split-Path -Parent $winterExe).TrimEnd('\')
$pathEntries = @($persistedUserPath, $persistedMachinePath) |
    Where-Object { $_ } |
    ForEach-Object { $_ -split ';' } |
    ForEach-Object { $_.Trim().TrimEnd('\') }
if ($pathEntries -notcontains $winterDir) {
    Write-Warning "$winterDir is not on your persisted PATH. Add it and restart your shell before running 'winter'."
}

Write-Host 'Installing the Windows Terminal integration...' -ForegroundColor Cyan
if ($NoAutostart) {
    & $winterExe install --no-autostart
} else {
    & $winterExe install
}
if ($LASTEXITCODE -ne 0) {
    throw "winter install failed with exit code $LASTEXITCODE"
}

if ($NoAutostart) {
    Write-Host ''
    Write-Host "Done. Run 'winter' after every restart: autostart was skipped because of -NoAutostart." -ForegroundColor Yellow
    return
}

# The install already registered the logon task; read it back so the summary
# reflects persisted state instead of the request.
Write-Host 'Verifying persistent autostart...' -ForegroundColor Cyan
$autostartJson = & $winterExe autostart status | Out-String
$autostart = $autostartJson | ConvertFrom-Json

if ($autostart.registered) {
    Write-Host ''
    Write-Host 'Done. The controller now starts automatically at sign-in and survives restarts.' -ForegroundColor Green
    Write-Host "  task        : $($autostart.taskName)" -ForegroundColor DarkGray
    Write-Host "  command     : $($autostart.command)" -ForegroundColor DarkGray
    Write-Host "  run level   : $($autostart.runLevel)" -ForegroundColor DarkGray
    Write-Host "  stop it now : winter autostart disable" -ForegroundColor DarkGray
} else {
    Write-Warning "Persistent autostart is not active ($($autostart.status)). Run 'winter autostart enable' from an elevated shell to finish the setup."
    Write-Host "Run 'winter' to start the controller now." -ForegroundColor Green
}
