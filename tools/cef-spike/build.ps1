param(
    [string]$Cargo = "check",
    [switch]$Release
)
# Compile-check or build the CEF spike with the toolchain CEF needs.
#
# cef-dll-sys runs cmake with the Ninja generator, and neither tool is on PATH by
# default on this machine, so both are prepended here rather than asking the
# caller to install them globally.
$ErrorActionPreference = 'Stop'

$cmakeBin = "C:\Program Files\CMake\bin"
$ninjaBin = Join-Path $env:LOCALAPPDATA "Microsoft\WinGet\Packages\Ninja-build.Ninja_Microsoft.Winget.Source_8wekyb3d8bbwe"

foreach ($tool in @($cmakeBin, $ninjaBin)) {
    if (Test-Path $tool) { $env:PATH = "$tool;$env:PATH" }
}

if (-not (Get-Command cmake -ErrorAction SilentlyContinue)) {
    throw "cmake not found. Install it: winget install Kitware.CMake"
}
if (-not (Get-Command ninja -ErrorAction SilentlyContinue)) {
    throw "ninja not found. Install it: winget install Ninja-build.Ninja"
}

Push-Location $PSScriptRoot
try {
    $args = @('build')
    if ($Cargo -eq 'check') { $args = @('check') }
    if ($Release) { $args += '--release' }
    Write-Host "cargo $($args -join ' ')"
    & cargo @args
    exit $LASTEXITCODE
}
finally {
    Pop-Location
}
