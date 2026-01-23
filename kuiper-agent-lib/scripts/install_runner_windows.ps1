# GitHub Actions Runner installation script for Windows
# Usage: install_runner_windows.ps1 -RunnerDir <path> -Url <url>
param(
    [Parameter(Mandatory=$true)][string]$RunnerDir,
    [Parameter(Mandatory=$true)][string]$Url
)

$ErrorActionPreference = 'Stop'

Write-Output "Creating runner directory..."
New-Item -ItemType Directory -Force -Path $RunnerDir | Out-Null
Set-Location $RunnerDir

Write-Output "Downloading runner from $Url..."
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
Invoke-WebRequest -Uri $Url -OutFile 'actions-runner.zip' -UseBasicParsing

Write-Output "Extracting runner..."
Expand-Archive -Path 'actions-runner.zip' -DestinationPath '.' -Force
Remove-Item 'actions-runner.zip'

Write-Output "Runner installed successfully"
