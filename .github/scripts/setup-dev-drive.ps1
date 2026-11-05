# Configure a fast drive for Windows CI jobs.
#
# GitHub-hosted Windows runners do not always expose a secondary D: volume, and
# older runner images (for example, windows-2022) do not support Dev Drives at
# all. Use a Dev Drive when the image supports one, otherwise route build I/O
# through the existing secondary volume, falling back to C: only as a last
# resort. CI_BUILD_ROOT must always be defined.

function Test-DevDrive {
    param([string]$Drive)

    & fsutil devdrv query $Drive *> $null
    return $LASTEXITCODE -eq 0
}

function Test-DevDriveSupport {
    $formatVolume = Get-Command Format-Volume -ErrorAction SilentlyContinue
    return $null -ne $formatVolume -and $formatVolume.Parameters.ContainsKey("DevDrive")
}

function Invoke-BestEffort {
    param([scriptblock]$Script, [string]$Description)

    try {
        & $Script
    } catch {
        Write-Warning "$Description failed: $($_.Exception.Message)"
    }
}

$DevDriveSupported = Test-DevDriveSupport

if ((Test-Path "D:\") -and (Test-DevDrive "D:")) {
    Write-Output "Using existing Dev Drive at D:"
    $Drive = "D:"
} elseif (-not $DevDriveSupported) {
    # Format-Volume on this image has no -DevDrive parameter, so provisioning a
    # VHD cannot produce a Dev Drive. The existing D: volume still keeps build
    # I/O off the OS disk, which is the main goal.
    if (Test-Path "D:\") {
        Write-Output "Dev Drives are not supported on this runner; using existing D: volume."
        $Drive = "D:"
    } else {
        Write-Warning "Dev Drives are not supported on this runner and no D: volume is available; falling back to C:."
        $Drive = "C:"
    }
} else {
    if (Test-Path "D:\") {
        Write-Output "Existing D: volume is not a Dev Drive; provisioning a new Dev Drive VHD."
    }

    try {
        $VhdPath = Join-Path $env:RUNNER_TEMP "codex-dev-drive.vhdx"
        $SizeBytes = 64GB

        if (Test-Path $VhdPath) {
            Remove-Item -Path $VhdPath -Force
        }

        New-VHD -Path $VhdPath -SizeBytes $SizeBytes -Dynamic -ErrorAction Stop | Out-Null
        $Mounted = Mount-VHD -Path $VhdPath -Passthru -ErrorAction Stop
        $Disk = $Mounted | Get-Disk -ErrorAction Stop
        $Disk | Initialize-Disk -PartitionStyle GPT -ErrorAction Stop
        $Partition = $Disk | New-Partition -AssignDriveLetter -UseMaximumSize -ErrorAction Stop
        $Volume = $Partition | Format-Volume -FileSystem ReFS -NewFileSystemLabel "CodexDevDrive" -DevDrive -Confirm:$false -Force -ErrorAction Stop

        $Drive = "$($Volume.DriveLetter):"

        if (-not (Test-DevDrive $Drive)) {
            throw "Provisioned volume at $Drive did not pass Dev Drive verification."
        }

        Invoke-BestEffort { fsutil devdrv trust $Drive } "Trusting Dev Drive $Drive"
        Invoke-BestEffort { fsutil devdrv enable /disallowAv } "Disabling AV filter attachment for Dev Drives"

        Write-Output "Using Dev Drive at $Drive"
    } catch {
        $Message = "Failed to create a Dev Drive: $($_.Exception.Message)"
        if (Test-Path "D:\") {
            Write-Warning "$Message Using existing D: volume."
            $Drive = "D:"
        } else {
            Write-Warning "$Message Falling back to C:."
            $Drive = "C:"
        }
    }
}

"CI_BUILD_ROOT=$Drive" | Out-File -FilePath $env:GITHUB_ENV -Encoding utf8 -Append
