param(
    [Parameter(Mandatory = $true)]
    [string]$ExePath
)

$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrWhiteSpace($env:WINDOWS_CERT_PFX_B64)) {
    Write-Host "Skipping Authenticode signing for $ExePath (WINDOWS_CERT_PFX_B64 secret not set)."
    exit 0
}

if ([string]::IsNullOrEmpty($env:WINDOWS_CERT_PASSWORD)) {
    throw 'WINDOWS_CERT_PASSWORD secret is required when WINDOWS_CERT_PFX_B64 is set.'
}

if (-not (Test-Path -LiteralPath $ExePath)) {
    throw "Executable not found: $ExePath"
}

$signtool = (Get-Command signtool.exe -ErrorAction SilentlyContinue | Select-Object -First 1).Source
if (-not $signtool) {
    $candidates = Get-ChildItem -Path 'C:\Program Files (x86)\Windows Kits\10\bin\*\x64\signtool.exe' -ErrorAction SilentlyContinue |
        Sort-Object -Property FullName -Descending
    if ($candidates) {
        $signtool = $candidates[0].FullName
    }
}
if (-not $signtool) {
    throw 'signtool.exe not found in PATH or Windows Kits.'
}

$tempRoot = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
$pfxPath = Join-Path $tempRoot ("codesign-" + [guid]::NewGuid().ToString('N') + '.pfx')

try {
    $cleanB64 = ($env:WINDOWS_CERT_PFX_B64 -replace '\s+', '')
    [IO.File]::WriteAllBytes($pfxPath, [Convert]::FromBase64String($cleanB64))

    $timestamps = @(
        'http://timestamp.digicert.com',
        'http://timestamp.sectigo.com'
    )
    $signed = $false
    foreach ($tsa in $timestamps) {
        & $signtool sign /fd SHA256 /td SHA256 /tr $tsa /f $pfxPath /p $env:WINDOWS_CERT_PASSWORD $ExePath
        if ($LASTEXITCODE -eq 0) {
            $signed = $true
            break
        }
        Write-Warning "signtool failed with timestamp server $tsa (exit $LASTEXITCODE); retrying..."
    }
    if (-not $signed) {
        throw "signtool failed to sign $ExePath with all timestamp servers."
    }
} finally {
    Remove-Item -LiteralPath $pfxPath -Force -ErrorAction SilentlyContinue
}

$sig = Get-AuthenticodeSignature -FilePath $ExePath
if ($null -eq $sig.SignerCertificate -or $sig.Status -in @('NotSigned', 'HashMismatch', 'NotSupportedFileFormat')) {
    throw "Authenticode signature verification failed for ${ExePath}: status=$($sig.Status) message=$($sig.StatusMessage)"
}

Write-Host "Signed $ExePath with $($sig.SignerCertificate.Subject) (thumbprint $($sig.SignerCertificate.Thumbprint), status=$($sig.Status))"
