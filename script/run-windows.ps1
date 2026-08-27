#Requires -Version 5.1
<#
.SYNOPSIS
    Build and run Genesi Code on Windows.

.DESCRIPTION
    Windows is a dev/test target here, not the shipping one (Genesi OS is
    CachyOS-based), so this wraps the few things that differ:

      * Debug info is turned OFF for the build. The `warp` crate is monolithic
        with ~200 features, and linking it WITH debug info exhausts memory on a
        32 GB machine -- rustc dies with "LLVM ERROR: out of memory", or with a
        bare `STATUS_STACK_BUFFER_OVERRUN` (0xc0000409) and no diagnostic at
        all, which is how a Rust allocation-failure abort surfaces on Windows.
        That failure looks exactly like a compile error and is not one.

      * A running instance is closed first, so the new binary is not blocked by
        a locked .exe.

    Local AI on Windows: `genesi-ai-turbo` (the GGUF/MoE backend) is Linux-only,
    so Turbo mode will not work here. Ollama does -- start it with `ollama serve`
    and pick an Ollama tag. Cloud (BYOK) providers are plain HTTP and work.

.PARAMETER Pull
    git pull --ff-only before building.

.PARAMETER NoBuild
    Skip the build and just launch what is already in target\debug.

.PARAMETER NoRun
    Build only; do not launch.

.PARAMETER FreshOnboarding
    Clear the "onboarding completed" flag so the next launch shows the first-run
    slides again. The flag is a private preference, which on Windows means the
    registry rather than a config file, and it is written as soon as the last
    slide's button is pressed -- so a single run through the flow is enough to
    stop it appearing forever.

.EXAMPLE
    .\script\run-windows.ps1 -Pull
#>
[CmdletBinding()]
param(
    [switch]$Pull,
    [switch]$NoBuild,
    [switch]$NoRun,
    [switch]$FreshOnboarding
)

$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$exePath = Join-Path $repoRoot 'target\debug\warp-oss.exe'
$binName = 'warp-oss'

Write-Host "Genesi Code - Windows dev run" -ForegroundColor Cyan
Write-Host "repo: $repoRoot"

if ($Pull) {
    Write-Host "`n[1/3] git pull --ff-only" -ForegroundColor Yellow
    git -C $repoRoot pull --ff-only
    if ($LASTEXITCODE -ne 0) { throw "git pull failed" }
}

# Close a running instance: Windows locks the .exe while it runs, so the link
# step would fail with a permission error instead of an obvious "it's open".
$running = Get-Process -Name $binName -ErrorAction SilentlyContinue
if ($running) {
    Write-Host "`nClosing running instance (PID $($running.Id -join ', '))" -ForegroundColor Yellow
    $running | Stop-Process -Force
    Start-Sleep -Seconds 2
}

if (-not $NoBuild) {
    Write-Host "`n[2/3] cargo build --bin $binName" -ForegroundColor Yellow

    # See the note above: both of these keep the link inside available memory.
    # Incremental compilation is off for the same reason -- it trades memory for
    # rebuild speed, and on this crate that trade is what tips rustc over.
    $env:CARGO_PROFILE_DEV_DEBUG = '0'
    $env:CARGO_INCREMENTAL = '0'

    $started = Get-Date
    & cargo build --manifest-path (Join-Path $repoRoot 'Cargo.toml') -p warp --bin $binName -j 2
    $exit = $LASTEXITCODE
    $elapsed = [math]::Round(((Get-Date) - $started).TotalMinutes, 1)

    if ($exit -ne 0) {
        Write-Host "`nBuild FAILED after $elapsed min (exit $exit)." -ForegroundColor Red
        Write-Host "If no file:line error was printed, rustc crashed rather than" -ForegroundColor Red
        Write-Host "rejecting the code -- that means out of memory. Check commit charge:" -ForegroundColor Red
        Write-Host "  Get-Counter '\Memory\Committed Bytes','\Memory\Commit Limit'" -ForegroundColor DarkGray
        Write-Host "and close memory-heavy apps (games, browsers) before retrying." -ForegroundColor Red
        exit $exit
    }
    Write-Host "Build OK in $elapsed min." -ForegroundColor Green
}

if ($FreshOnboarding) {
    # Only the OSS key: HKCU:\Software\Warp.dev\Warp belongs to an installed
    # Warp, and resetting that one is not ours to do.
    $key = 'HKCU:\Software\Warp.dev\WarpOss'
    if (Test-Path $key) {
        Remove-ItemProperty -Path $key -Name HasCompletedOnboarding -ErrorAction SilentlyContinue
        Remove-ItemProperty -Path $key -Name HasCompletedHOAOnboarding -ErrorAction SilentlyContinue
        Write-Host "`nOnboarding flag cleared - next launch starts at the first slide." -ForegroundColor Yellow
    }
}

if (-not (Test-Path $exePath)) { throw "binary not found: $exePath (build it first)" }

if ($NoRun) {
    Write-Host "`nBuilt, not launching (-NoRun)." -ForegroundColor Green
    exit 0
}

Write-Host "`n[3/3] launching" -ForegroundColor Yellow
$proc = Start-Process -FilePath $exePath -PassThru
Start-Sleep -Seconds 8

$alive = Get-Process -Id $proc.Id -ErrorAction SilentlyContinue
if ($alive) {
    $mb = [math]::Round($alive.WorkingSet64 / 1MB)
    Write-Host "Running - PID $($alive.Id), $mb MB." -ForegroundColor Green
} else {
    Write-Host "Exited immediately (code $($proc.ExitCode))." -ForegroundColor Red
    exit 1
}
