$ErrorActionPreference = 'Stop'

$packageArgs = @{
  packageName    = $env:ChocolateyPackageName
  unzipLocation  = "$(Split-Path -Parent $MyInvocation.MyCommand.Definition)"
  url64bit       = "https://github.com/muimsd/tilefeed/releases/download/v$($env:ChocolateyPackageVersion)/tilefeed-x86_64-pc-windows-msvc.zip"
  checksum64     = 'ec838a07abbb58b5f5652788ee96a4dbf011592698f5711ef431003a304fd8a7'
  checksumType64 = 'sha256'
}

Install-ChocolateyZipPackage @packageArgs
