$ErrorActionPreference = 'Stop'

$packageArgs = @{
  packageName    = $env:ChocolateyPackageName
  unzipLocation  = "$(Split-Path -Parent $MyInvocation.MyCommand.Definition)"
  url64bit       = "https://github.com/muimsd/tilefeed/releases/download/v$($env:ChocolateyPackageVersion)/tilefeed-x86_64-pc-windows-msvc.zip"
  checksum64     = 'e1520b9faf47c76143c60f98860ab6eeb00b138949ebbd6645604e6c520e8207'
  checksumType64 = 'sha256'
}

Install-ChocolateyZipPackage @packageArgs
