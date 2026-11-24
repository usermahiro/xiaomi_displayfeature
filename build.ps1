$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$NdkHome = if ($env:ANDROID_NDK_HOME) { $env:ANDROID_NDK_HOME } else { "C:\Users\1215\Desktop\android-ndk-r26b" }
$NdkBin = Join-Path $NdkHome "toolchains\llvm\prebuilt\windows-x86_64\bin"
$DevkitBin = "C:\Users\1215\Desktop\w64devkit\bin"

if (Test-Path $DevkitBin) {
    $env:PATH = "$DevkitBin;$env:PATH"
}

if (-not (Test-Path $NdkBin)) {
    throw "Android NDK toolchain not found at $NdkBin. Please set ANDROID_NDK_HOME."
}

$env:CC_aarch64_linux_android = Join-Path $NdkBin "aarch64-linux-android33-clang.cmd"
$env:CXX_aarch64_linux_android = Join-Path $NdkBin "aarch64-linux-android33-clang++.cmd"
$env:AR_aarch64_linux_android = Join-Path $NdkBin "llvm-ar.exe"
$env:CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER = Join-Path $NdkBin "aarch64-linux-android33-clang.cmd"
$env:RUSTFLAGS = "-C link-arg=-Wl,-export-dynamic"

$CargoBin = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
if (-not (Test-Path $CargoBin)) {
    $CargoBin = "cargo"
}

Write-Host "=== Building Xiaomi DisplayFeature AIDL Mock (aarch64-linux-android) ===" -ForegroundColor Cyan
Push-Location $ScriptDir
try {
    & $CargoBin build --target aarch64-linux-android --release
    if ($LASTEXITCODE -ne 0) {
        throw "Cargo build failed with exit code $LASTEXITCODE"
    }

    $BinPath = Join-Path $ScriptDir "target\aarch64-linux-android\release\vendor_xiaomi_hardware_displayfeature_service"
    $StripBin = Join-Path $NdkBin "llvm-strip.exe"
    if (Test-Path $StripBin) {
        Write-Host "Stripping release binary with llvm-strip..." -ForegroundColor Yellow
        & $StripBin $BinPath
    }

    $OutDir = Join-Path $ScriptDir "out"
    if (Test-Path $OutDir) { Remove-Item $OutDir -Recurse -Force }
    New-Item -ItemType Directory -Force -Path (Join-Path $OutDir "system\vendor\bin\hw") | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $OutDir "system\vendor\etc\init") | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $OutDir "system\vendor\etc\vintf\manifest") | Out-Null

    $TargetBinary = Join-Path $OutDir "system\vendor\bin\hw\vendor.xiaomi.hardware.displayfeature-service"
    Copy-Item $BinPath $TargetBinary -Force

    function Set-UnixText($path, $text) {
        $clean = $text.Replace("`r`n", "`n")
        [System.IO.File]::WriteAllBytes($path, [System.Text.Encoding]::UTF8.GetBytes($clean))
    }

    # 1. Init RC
    $RcContent = @"
service vendor.xiaomi.hardware.displayfeature-service /vendor/bin/hw/vendor.xiaomi.hardware.displayfeature-service
    class hal
    user root
    group root system system_ext
"@
    Set-UnixText (Join-Path $OutDir "system\vendor\etc\init\vendor.xiaomi.hardware.displayfeature-service.rc") $RcContent

    # 2. VINTF Manifest
    $XmlContent = @"
<manifest version="1.0" type="device">
    <hal format="aidl">
        <name>vendor.xiaomi.hardware.displayfeature_aidl</name>
        <version>2</version>
        <fqname>IDisplayFeature/default</fqname>
    </hal>
</manifest>
"@
    Set-UnixText (Join-Path $OutDir "system\vendor\etc\vintf\manifest\vendor.xiaomi.hardware.displayfeature.xml") $XmlContent

    # 3. module.prop
    $PropContent = @"
id=hyperos_displayfeature_mock_transsion
name=HyperOS DisplayFeature Mock (Transsion / MT6789)
version=1.0.0
versionCode=100
author=Antigravity
description=Provides vendor.xiaomi.hardware.displayfeature_aidl AIDL HAL mock service to satisfy HyperOS/MIUI framework display queries and eliminate retry logcat spam.
"@
    Set-UnixText (Join-Path $OutDir "module.prop") $PropContent

    # 4. service.sh (Robust bootloader startup script with Unix LF)
    $ServiceSh = @"
#!/system/bin/sh
MODDIR=`${0%/*}

# Wait for boot completion
while [ "`$(getprop sys.boot_completed)" != "1" ]; do
    sleep 2
done

# Give services time to stabilize
sleep 2

# Start service if not already running
if ! pgrep -f "vendor.xiaomi.hardware.displayfeature-service" > /dev/null; then
    if [ -x "`$MODDIR/system/vendor/bin/hw/vendor.xiaomi.hardware.displayfeature-service" ]; then
        "`$MODDIR/system/vendor/bin/hw/vendor.xiaomi.hardware.displayfeature-service" > /data/local/tmp/displayfeature.log 2>&1 &
    elif [ -x /vendor/bin/hw/vendor.xiaomi.hardware.displayfeature-service ]; then
        /vendor/bin/hw/vendor.xiaomi.hardware.displayfeature-service > /data/local/tmp/displayfeature.log 2>&1 &
    fi
fi
"@
    Set-UnixText (Join-Path $OutDir "service.sh") $ServiceSh

    # 5. Magisk / KernelSU clean ZIP packaging (forward slashes, proper zip structure)
    $ZipPath = "C:\Users\1215\Desktop\platform-tools\HyperOS_DisplayFeature_Mock.zip"
    if (Test-Path $ZipPath) { Remove-Item $ZipPath -Force }

    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem

    $zipArchive = [System.IO.Compression.ZipFile]::Open($ZipPath, [System.IO.Compression.ZipArchiveMode]::Create)
    try {
        Get-ChildItem -Path $OutDir -Recurse -File | ForEach-Object {
            $relPath = $_.FullName.Substring($OutDir.Length + 1).Replace("\", "/")
            $entry = $zipArchive.CreateEntry($relPath, [System.IO.Compression.CompressionLevel]::Optimal)
            $entryStream = $entry.Open()
            try {
                $fileStream = [System.IO.File]::OpenRead($_.FullName)
                try {
                    $fileStream.CopyTo($entryStream)
                } finally {
                    $fileStream.Dispose()
                }
            } finally {
                $entryStream.Dispose()
            }
        }
    } finally {
        $zipArchive.Dispose()
    }

    Write-Host "Build succeeded! Module zip created at $ZipPath" -ForegroundColor Green
}
finally {
    Pop-Location
}
