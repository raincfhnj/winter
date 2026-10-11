<#
.SYNOPSIS
    One-time setup for Winter.

.DESCRIPTION
    Builds the release binaries, installs the interactive `winter` command into
    the Cargo bin directory, deploys the executables the controller runs to a
    machine-level directory only administrators can write, and runs
    `winter install` from that protected copy.

    Why the protected copy exists
    -----------------------------
    The logon task Winter registers starts `winterd.exe` at the highest
    available run level at every sign-in, and registering it needs administrator
    approval only once. Registering a task whose executable lives in a
    user-writable directory - `%USERPROFILE%\.cargo\bin` is one - would let any
    process running as this account replace the controller and have the
    replacement started elevated at the next sign-in. `winter autostart enable`
    therefore refuses to persist an executable it cannot prove is protected, so
    the install has to stage the binaries somewhere an unprivileged process
    cannot write: `%ProgramFiles%\Winter`.

    The Cargo bin copy stays as the interactive entry point, because that is
    where `winter` has always been and it works for `winter doctor`, `winter
    ui`, and friends. The trade-off is explicit: that copy cannot be persisted,
    and `winter autostart status` reports `locationProtected: false` whenever it
    is the one asking. New shells resolve `winter` to the protected copy,
    because a Machine PATH entry precedes the per-user Cargo bin directory.

    This script requires administrator rights and stops with an error when it
    does not have them: an install that silently lands in a location that cannot
    persist would be a worse outcome than a failed install. It is idempotent -
    re-running overwrites the same files and adds no duplicate PATH entry - and
    it rolls the deployment back when `winter install` fails.

.EXAMPLE
    PS> .\install.ps1

.EXAMPLE
    PS> .\install.ps1 -NoAutostart
#>
[CmdletBinding()]
param(
    # Deploy the binaries and install the Windows Terminal integration, but
    # leave the controller out of the sign-in sequence.
    [switch]$NoAutostart
)

Set-StrictMode -Version Latest
# Keep native-command exit codes observable through $LASTEXITCODE instead of
# turning them into errors, so the friendly `throw`s below keep working under
# PowerShell 7.3+ profiles that set $ErrorActionPreference = 'Stop'.
$PSNativeCommandUseErrorActionPreference = $false

$ErrorActionPreference = 'Stop'

function Test-Elevated {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    (New-Object Security.Principal.WindowsPrincipal($identity)).IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Get-MachinePath {
    [Environment]::GetEnvironmentVariable('Path', 'Machine')
}

function Split-PathList {
    param([string]$Value)
    @($Value -split ';' | ForEach-Object { $_.Trim().TrimEnd('\') } | Where-Object { $_ })
}

function Test-PathContains {
    param([string]$Value, [string]$Entry)
    (Split-PathList $Value) -contains $Entry.TrimEnd('\')
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw 'Rust/Cargo was not found. Install it from https://rustup.rs and run this script again.'
}

if (-not (Test-Elevated)) {
    throw @'
Administrator rights are required. The controller must be installed in a directory only
administrators can write (the logon task starts it elevated at every sign-in), and copying
there, editing the machine PATH, and registering that task all need approval.

Open PowerShell with 'Run as administrator' and run this script again. Nothing was changed.
'@
}

$programFiles = $env:ProgramFiles
if (-not $programFiles) {
    throw 'ProgramFiles is not defined in this environment; the protected install directory cannot be determined.'
}

$repoRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$releaseDir = Join-Path $repoRoot 'target\release'
$installDir = Join-Path $programFiles 'Winter'
$backupDir = Join-Path $installDir '.rollback'
$binaries = @('winter.exe', 'winterminalp.exe', 'winterd.exe')

Push-Location $repoRoot
try {
    Write-Host 'Building winter, winterminalp (compatibility alias), and winterd (release)...' -ForegroundColor Cyan
    cargo build --release --bins --locked
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed with exit code $LASTEXITCODE"
    }

    foreach ($name in $binaries) {
        $built = Join-Path $releaseDir $name
        if (-not (Test-Path -LiteralPath $built)) {
            throw "the release build did not produce $built"
        }
    }

    # Keep the familiar interactive entry point on the user PATH. `winterd.exe`
    # is deliberately not installed here: it is the executable the persisted
    # task starts, and a user-writable copy of it would be the very exposure
    # this layout removes.
    Write-Host 'Installing the interactive winter command into the Cargo bin directory...' -ForegroundColor Cyan
    cargo install --path . --bin winter --bin winterminalp --locked --force
    if ($LASTEXITCODE -ne 0) {
        throw "cargo install failed with exit code $LASTEXITCODE"
    }
}
finally {
    Pop-Location
}

# Reload the persisted User+Machine PATH so the Cargo bin copy is locatable in
# this process. The persisted PATH itself is only touched for the protected
# directory, below, and only when it is missing there.
$persistedUserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$persistedMachinePath = Get-MachinePath
$env:Path = (@($env:Path, $persistedUserPath, $persistedMachinePath) | Where-Object { $_ }) -join ';'

$installDirExisted = Test-Path -LiteralPath $installDir
$pathEntryAdded = $false
$machinePathBefore = $persistedMachinePath

Write-Host "Deploying the controller to $installDir ..." -ForegroundColor Cyan
try {
    New-Item -ItemType Directory -Force -Path $installDir | Out-Null

    # One backup of the previous binaries, so a failed `winter install` can put
    # the machine back the way it was. Re-running the script replaces it.
    if ($installDirExisted) {
        New-Item -ItemType Directory -Force -Path $backupDir | Out-Null
        foreach ($name in $binaries) {
            $existing = Join-Path $installDir $name
            if (Test-Path -LiteralPath $existing) {
                Copy-Item -LiteralPath $existing -Destination (Join-Path $backupDir $name) -Force
            }
        }
    }

    foreach ($name in $binaries) {
        Copy-Item -LiteralPath (Join-Path $releaseDir $name) -Destination (Join-Path $installDir $name) -Force
        if (-not (Test-Path -LiteralPath (Join-Path $installDir $name))) {
            throw "the deployment did not create $(Join-Path $installDir $name)"
        }
    }

    if (-not (Test-PathContains $machinePathBefore $installDir)) {
        $existingEntries = Split-PathList $machinePathBefore
        $updated = (@($existingEntries) + @($installDir) | Where-Object { $_ }) -join ';'
        [Environment]::SetEnvironmentVariable('Path', $updated, 'Machine')
        $pathEntryAdded = $true
    }

    $protectedWinter = Join-Path $installDir 'winter.exe'
    Write-Host 'Installing the Windows Terminal integration from the protected copy...' -ForegroundColor Cyan
    if ($NoAutostart) {
        & $protectedWinter install --no-autostart
    }
    else {
        & $protectedWinter install
    }
    if ($LASTEXITCODE -ne 0) {
        throw "winter install failed with exit code $LASTEXITCODE"
    }
}
catch {
    $failure = $_
    Write-Warning 'The install failed; restoring the previous state.'
    if ($pathEntryAdded) {
        [Environment]::SetEnvironmentVariable('Path', $machinePathBefore, 'Machine')
    }
    if ($installDirExisted) {
        if (Test-Path -LiteralPath $backupDir) {
            foreach ($name in $binaries) {
                $backup = Join-Path $backupDir $name
                if (Test-Path -LiteralPath $backup) {
                    Copy-Item -LiteralPath $backup -Destination (Join-Path $installDir $name) -Force
                }
            }
            # The previous binaries are back in place; a second copy of them
            # inside the install directory would only be picked up as the next
            # run's own backup.
            Remove-Item -LiteralPath $backupDir -Recurse -Force
        }
    }
    elseif (Test-Path -LiteralPath $installDir) {
        Remove-Item -LiteralPath $installDir -Recurse -Force
    }
    throw $failure
}

if (Test-Path -LiteralPath $backupDir) {
    Remove-Item -LiteralPath $backupDir -Recurse -Force
}

$winterExe = Join-Path $installDir 'winter.exe'

if ($NoAutostart) {
    Write-Host ''
    Write-Host "Done. Run 'winter' after every restart: autostart was skipped because of -NoAutostart." -ForegroundColor Yellow
    return
}

# The install already registered the logon task; read it back so the summary
# reflects persisted state instead of the request.
Write-Host 'Verifying persistent autostart...' -ForegroundColor Cyan
$autostart = (& $winterExe autostart status | Out-String) | ConvertFrom-Json

if ($autostart.registered -and $autostart.locationProtected) {
    Write-Host ''
    Write-Host 'Done. The controller now starts automatically at sign-in and survives restarts.' -ForegroundColor Green
    Write-Host "  task        : $($autostart.taskName)" -ForegroundColor DarkGray
    Write-Host "  command     : $($autostart.command)" -ForegroundColor DarkGray
    Write-Host "  run level   : $($autostart.runLevel)" -ForegroundColor DarkGray
    Write-Host "  stop it now : winter autostart disable" -ForegroundColor DarkGray
}
elseif ($autostart.registered) {
    Write-Warning "A logon task is registered, but the copy that just ran is not in a protected location, so the task was not pointed at it. $($autostart.locationIssue)"
    Write-Host "Run '$winterExe autostart enable' from an elevated shell to repair the task." -ForegroundColor Yellow
}
else {
    Write-Warning "Persistent autostart is not active ($($autostart.status)). $($autostart.locationIssue)"
    Write-Host "Run '$winterExe autostart enable' from an elevated shell to finish the setup." -ForegroundColor Yellow
}

Write-Host ''
Write-Host "Interactive entry point: $winterExe (new shells resolve 'winter' there)." -ForegroundColor DarkGray
Write-Host "The Cargo bin copy is a convenience only: it cannot persist the controller, and 'winter autostart status' will report locationProtected: false when it is the one asking." -ForegroundColor DarkGray
