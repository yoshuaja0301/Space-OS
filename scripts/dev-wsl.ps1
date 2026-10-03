#requires -Version 7.3
[CmdletBinding()]
param(
    [string]$Distribution = 'SpaceOS-D01',
    [string]$CargoHome = '/opt/spaceos/cargo',
    [string]$RustupHome = '/opt/spaceos/rustup',
    [Parameter(Position = 0, ValueFromRemainingArguments = $true)]
    [string[]]$XtaskArgs = @('build')
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandArgumentPassing = 'Standard'
$PSNativeCommandUseErrorActionPreference = $false
$repoRoot = Split-Path -Parent $PSScriptRoot
$linuxPath = "${CargoHome}/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

& wsl.exe --distribution $Distribution --cd $repoRoot --exec /usr/bin/env `
    "CARGO_HOME=$CargoHome" "RUSTUP_HOME=$RustupHome" "PATH=$linuxPath" `
    "${CargoHome}/bin/cargo" xtask @XtaskArgs
exit $LASTEXITCODE
