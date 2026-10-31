# GitHub Actions Runner installation script for Windows
# Usage: install_runner_windows.ps1 -RunnerDir <path> -Url <url> [-Sha256 <checksum>]
# GitHub publishes checksums in its release notes, not next to the archive, so one is passed in.
#
# Everything from $ErrorActionPreference on is sent with its lines joined by "; ": no comments
# below here, and keep blocks on one line.
param(
    [Parameter(Mandatory=$true)][string]$RunnerDir,
    [Parameter(Mandatory=$true)][string]$Url,
    [string]$Sha256 = ''
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

Write-Output "Creating runner directory..."
New-Item -ItemType Directory -Force -Path $RunnerDir | Out-Null
Set-Location $RunnerDir

Write-Output "Downloading runner from $Url..."
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
Invoke-WebRequest -Uri $Url -OutFile 'actions-runner.zip' -UseBasicParsing

if ($Sha256) { $Actual = (Get-FileHash -Algorithm SHA256 -LiteralPath 'actions-runner.zip').Hash.ToLower(); if ($Actual -ne $Sha256) { Remove-Item -Force -LiteralPath 'actions-runner.zip'; throw "Checksum mismatch for ${Url}: expected $Sha256, got $Actual" }; Write-Output "Checksum OK" }

Write-Output "Extracting runner..."
Expand-Archive -Path 'actions-runner.zip' -DestinationPath '.' -Force
Remove-Item 'actions-runner.zip'

Write-Output "Runner installed successfully"
