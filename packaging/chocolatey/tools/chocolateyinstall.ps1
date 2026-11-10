$ErrorActionPreference = 'Stop'

$packageArgs = @{
  packageName    = $env:ChocolateyPackageName
  unzipLocation  = "$(Split-Path -Parent $MyInvocation.MyCommand.Definition)"
  url64bit       = "https://github.com/muimsd/tilefeed/releases/download/v$($env:ChocolateyPackageVersion)/tilefeed-x86_64-pc-windows-msvc.zip"
  checksum64     = '9cc212d6d86905b382c664a8ecab015056d2411000e33a8331ef2f888c3999b5'
  checksumType64 = 'sha256'
}

Install-ChocolateyZipPackage @packageArgs
