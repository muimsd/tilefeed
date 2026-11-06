$ErrorActionPreference = 'Stop'

$packageArgs = @{
  packageName    = $env:ChocolateyPackageName
  unzipLocation  = "$(Split-Path -Parent $MyInvocation.MyCommand.Definition)"
  url64bit       = "https://github.com/muimsd/tilefeed/releases/download/v$($env:ChocolateyPackageVersion)/tilefeed-x86_64-pc-windows-msvc.zip"
  checksum64     = '991507e0e3eec3de222921c524ecf0f1dcbe2228f1f4b3e2ea5e4d88a82fe23b'
  checksumType64 = 'sha256'
}

Install-ChocolateyZipPackage @packageArgs
