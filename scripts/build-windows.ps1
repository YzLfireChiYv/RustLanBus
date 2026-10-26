# Explicit local GUI + CLI candidate assembly. No installer, publication or OS changes.
[CmdletBinding()]
param(
    [switch]$Build,
    [string]$OutputDirectory
)

$ErrorActionPreference = 'Stop'
$repoPath = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
if (-not $Build) {
    Write-Output 'Planned: build the desktop and headless CLI, then assemble a new local candidate folder.'
    Write-Output 'Run with -Build only when distribution artifacts are explicitly wanted.'
    return
}
if ($env:OS -ne 'Windows_NT') { throw 'This entry builds the Windows application.' }
if (-not $OutputDirectory) {
    $OutputDirectory = Join-Path $repoPath ('dist\RustLanBus-windows-' + (Get-Date -Format 'yyyyMMdd-HHmmss'))
}
$destinationPath = [IO.Path]::GetFullPath($OutputDirectory)
if (Test-Path -LiteralPath $destinationPath) { throw 'Choose a new output directory; existing candidates are preserved.' }
$desktopPath = Join-Path $repoPath 'apps\desktop'
$tauriPath = Join-Path $desktopPath 'node_modules\.bin\tauri.cmd'
if (-not (Test-Path -LiteralPath $tauriPath)) {
    throw 'Install the locked desktop dependencies with npm ci in apps/desktop first.'
}
$targetPath = Join-Path $repoPath 'target'
$previousTargetPath = $env:CARGO_TARGET_DIR
$previousRustFlags = $env:RUSTFLAGS
try {
    $env:CARGO_TARGET_DIR = $targetPath
    # Both portable entry points carry their native C runtime; WebView2 remains
    # the explicitly documented separate Windows runtime.
    $env:RUSTFLAGS = (($previousRustFlags + ' -C target-feature=+crt-static').Trim())
    Push-Location -LiteralPath $desktopPath
    try {
        & $tauriPath build --no-bundle
        if ($LASTEXITCODE -ne 0) { throw 'Desktop build failed.' }
    } finally { Pop-Location }
    Push-Location -LiteralPath $repoPath
    try {
        $hostLine = rustc -vV | Select-String '^host: (.+)$'
        if (-not $hostLine) { throw 'The native Rust target could not be identified.' }
        $nativeTarget = $hostLine.Matches[0].Groups[1].Value
        cargo build --release --locked --target $nativeTarget -p rustlanbus-engine --bin rustlanbus-engine
        if ($LASTEXITCODE -ne 0) { throw 'Headless CLI build failed.' }
    } finally { Pop-Location }
    New-Item -ItemType Directory -Path $destinationPath | Out-Null
    Copy-Item -LiteralPath (Join-Path $targetPath 'release\rustlanbus-ui.exe') -Destination (Join-Path $destinationPath 'RustLanBus.exe')
    Copy-Item -LiteralPath (Join-Path $targetPath "$nativeTarget\release\rustlanbus-engine.exe") -Destination $destinationPath
    Copy-Item -LiteralPath (Join-Path $repoPath 'LICENSE') -Destination $destinationPath
    Copy-Item -LiteralPath (Join-Path $repoPath 'LICENSE_EXCEPTION') -Destination $destinationPath
    foreach ($document in @('README.md', 'README.zh-CN.md', 'README.zh-TW.md', 'README.ja.md')) {
        Copy-Item -LiteralPath (Join-Path $repoPath $document) -Destination $destinationPath
    }
    $guidePath = Join-Path $destinationPath 'docs'
    New-Item -ItemType Directory -Path $guidePath | Out-Null
    foreach ($document in @(Get-ChildItem -LiteralPath (Join-Path $repoPath 'docs') -File | Where-Object { $_.Name -match '^(FIRST-USE|LATENCY|PLAN|README|ARCHITECTURE)(\.[a-zA-Z-]+)?\.md$' })) {
        Copy-Item -LiteralPath $document.FullName -Destination $guidePath
    }
    @'
RustLanBus Windows portable edition

Open RustLanBus.exe for the desktop. Windows requests administrator privileges.
The local interface uses Tauri, React and Microsoft Edge WebView2.
Audio acquisition, P2P transfer and playback run in the shared Rust core.

First use:
  1. Create the local configuration, then choose the system audio source and output.
  2. Route applications to a separate installed virtual playback device in Windows.
     Select that virtual playback device as the system loopback source.
     Select physical headphones/speakers as the main output.
  3. Create or join a room. On the listening computer, select members to hear.
Defaults: sending allowed; no members automatically heard; system publication on;
microphone off; local monitoring off; limiter off. Source and destination choices
are independent. Local monitoring works independently of room membership.

Window and background operation:
  X hides the window and leaves audio running. Left-click the system-tray icon
  to restore it. Choose Exit from its menu, or the window's Exit button, to stop.

Headless entry, from an administrator terminal:
  rustlanbus-engine.exe participant --help
Use participant init CONFIG --name NAME --bind 0.0.0.0:0 to create a configuration.
Use participant run CONFIG --stdin for interactive control; help lists commands.
Desktop and CLI share the core and configuration. One configuration runs in one process at a time.
The desktop uses the Microsoft Edge WebView2 Runtime. Audio drivers are installed separately.
Application preferences and room files remain outside this executable folder.
No firewall rule, login-startup task or audio routing is installed by extracting this folder.
Enable those application features explicitly when needed.

Source changes and build success do not establish runtime or listening results.
Actual application/UI/audio testing is owner-operated.
Public distribution uses the project's reviewed-source and license workflow.
'@ | Set-Content -LiteralPath (Join-Path $destinationPath 'README.txt') -Encoding utf8
    $candidateFiles = @(Get-ChildItem -LiteralPath $destinationPath -File -Recurse)
    $candidateFiles | ForEach-Object {
        '{0}  {1}' -f (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant(), ([IO.Path]::GetRelativePath($destinationPath, $_.FullName).Replace('\', '/'))
    } | Set-Content -LiteralPath (Join-Path $destinationPath 'SHA256SUMS.txt') -Encoding ascii
    Write-Output $destinationPath
} finally { $env:CARGO_TARGET_DIR = $previousTargetPath; $env:RUSTFLAGS = $previousRustFlags }
