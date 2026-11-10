$ErrorActionPreference = 'Stop'

$packageArgs = @{
  packageName    = $env:ChocolateyPackageName
  unzipLocation  = "$(Split-Path -Parent $MyInvocation.MyCommand.Definition)"
  url64bit       = "https://github.com/muimsd/tilefeed/releases/download/v$($env:ChocolateyPackageVersion)/tilefeed-x86_64-pc-windows-msvc.zip"
  checksum64     = '128fea175bcd21d4f91edf067fb32d6fde83d2683a69f66b1479ba9f4543927e'
  checksumType64 = 'sha256'
}

Install-ChocolateyZipPackage @packageArgs
