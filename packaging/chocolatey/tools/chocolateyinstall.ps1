$ErrorActionPreference = 'Stop'

$packageArgs = @{
  packageName    = $env:ChocolateyPackageName
  unzipLocation  = "$(Split-Path -Parent $MyInvocation.MyCommand.Definition)"
  url64bit       = "https://github.com/muimsd/tilefeed/releases/download/v$($env:ChocolateyPackageVersion)/tilefeed-x86_64-pc-windows-msvc.zip"
  checksum64     = '6654f203adc5d363d1373271c29b9b36c2bc00296ff0496c7f31989ddcfe4094'
  checksumType64 = 'sha256'
}

Install-ChocolateyZipPackage @packageArgs
