# Installs the newest leiser-harness on Windows, in PowerShell:
#
#   irm https://raw.githubusercontent.com/Leiser-AI/leiser-harness/development/install.ps1 | iex
#
# It downloads the installer of the latest release with PowerShell, which
# marks nothing as downloaded from the internet, so SmartScreen does not
# warn about an installer without a paid certificate. It installs for the
# current user, without admin rights, and starts the app. Run again to
# update; the app also updates itself.
#
# $env:LEISER_INSTALL_NO_OPEN = "1" does not start the app (used by the
# tests).

# Run in a script block of its own, so nothing it sets stays in the
# caller's session when piped into `iex`.
& {
  $ErrorActionPreference = "Stop"
  $ProgressPreference = "SilentlyContinue" # the progress bar slows the download

  $download = "https://github.com/Leiser-AI/leiser-harness/releases/latest/download/leiser-harness-windows-setup.exe"
  $installer = Join-Path ([System.IO.Path]::GetTempPath()) "leiser-harness-windows-setup.exe"

  Write-Output "Downloading $download ..."
  Invoke-WebRequest -Uri $download -OutFile $installer -UseBasicParsing

  # A running app would hold its files; the harness service keeps running and
  # takes the new version once no turn runs.
  Get-Process -Name "leiser-desktop" -ErrorAction SilentlyContinue | Stop-Process -Force

  Write-Output "Installing ..."
  $setup = Start-Process -FilePath $installer -ArgumentList "/S" -Wait -PassThru
  Remove-Item $installer -Force
  if ($setup.ExitCode -ne 0) {
    throw "The installer failed with exit code $($setup.ExitCode)."
  }

  $app = Join-Path $env:LOCALAPPDATA "leiser-harness\leiser-desktop.exe"
  if (-not (Test-Path $app)) {
    throw "The installer finished, but $app is missing."
  }
  Write-Output "Installed $app"
  if ($env:LEISER_INSTALL_NO_OPEN -ne "1") {
    Start-Process -FilePath $app
  }
}
