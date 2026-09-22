<#
.SYNOPSIS
  Installs, updates or removes glass-mic for the current Windows user.

.DESCRIPTION
  Install / update (default):
    1. Checks that VB-CABLE is installed ("CABLE Input" output device).
    2. Copies glass-mic.exe to %LOCALAPPDATA%\GlassMic (stops a running copy first).
    3. Allows inbound UDP for WebRTC in the Windows firewall (one UAC prompt, only if the rule
       is missing or points elsewhere).
    4. Registers a scheduled task: start at logon, plus a 5-minute watchdog that restarts
       glass-mic if it is not running. No admin rights needed.
    5. Runs `tailscale serve` so the iPad reaches https://<pc>.<tailnet>.ts.net/.
    6. Starts glass-mic and checks that it answers.

  -Uninstall reverses all of it and restores the previous default microphone.

  Re-running the script is safe; it updates an existing installation in place.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File .\install.ps1

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File .\install.ps1 -Uninstall
#>
[CmdletBinding()]
param(
    # glass-mic.exe to install. Default: next to this script, else target\release\glass-mic.exe.
    [string]$Exe,
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'GlassMic'),
    [string]$TaskName = 'GlassMic',
    [int]$Port = 8321,
    [int]$RtcPort = 8322,
    [string]$FirewallRuleName = 'Glass Mic (WebRTC UDP)',
    # Extra arguments for glass-mic, e.g. '--no-toast'.
    [string]$ExtraArgs = '',
    [switch]$SkipFirewall,
    [switch]$SkipTailscale,
    [switch]$Uninstall,
    # With -Uninstall: keep the log files.
    [switch]$KeepLogs
)

Set-StrictMode -Version 2
$ErrorActionPreference = 'Stop'

function Write-Step([string]$text) { Write-Host "==> $text" -ForegroundColor Cyan }
function Write-Ok([string]$text) { Write-Host "    $text" -ForegroundColor Green }
function Write-Note([string]$text) { Write-Host "    $text" -ForegroundColor Yellow }

$installedExe = Join-Path $InstallDir 'glass-mic.exe'
$logFile = Join-Path $InstallDir 'glass-mic.log'

function Stop-GlassMic {
    $procs = @(Get-CimInstance Win32_Process -Filter "Name='glass-mic.exe'" |
        Where-Object { $_.ExecutablePath -and ($_.ExecutablePath -ieq $installedExe) })
    foreach ($p in $procs) {
        Stop-Process -Id $p.ProcessId -Force -Confirm:$false -ErrorAction SilentlyContinue
        Write-Ok "stopped running glass-mic (pid $($p.ProcessId))"
    }
    if ($procs.Count -gt 0) { Start-Sleep -Milliseconds 800 }
}

# glass-mic is a GUI-subsystem program; `& glass-mic.exe` does not reliably hand its output to
# PowerShell 5.1. Redirecting into a file with Start-Process always works.
function Invoke-GlassMic([string]$path, [string[]]$arguments) {
    $out = [IO.Path]::GetTempFileName()
    try {
        $p = Start-Process -FilePath $path -ArgumentList $arguments -RedirectStandardOutput $out `
            -Wait -PassThru -NoNewWindow
        return [pscustomobject]@{ ExitCode = $p.ExitCode; Output = (Get-Content $out -Raw) }
    } finally {
        Remove-Item $out -ErrorAction SilentlyContinue
    }
}

function Invoke-Elevated([string]$command) {
    $isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator)
    if ($isAdmin) {
        Invoke-Expression $command
        return
    }
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($command))
    $p = Start-Process powershell -Verb RunAs -Wait -PassThru -WindowStyle Hidden `
        -ArgumentList '-NoProfile', '-EncodedCommand', $encoded
    if ($p.ExitCode -ne 0) { throw "elevated command failed with exit code $($p.ExitCode)" }
}

function Get-TailscaleName {
    $ts = Get-Command tailscale -ErrorAction SilentlyContinue
    if (-not $ts) { return $null }
    try {
        $status = & tailscale status --json 2>$null | ConvertFrom-Json
        return ($status.Self.DNSName).TrimEnd('.')
    } catch { return $null }
}

# ------------------------------------------------------------------------------------------------
if ($Uninstall) {
    Write-Step "Stopping and removing the scheduled task '$TaskName'"
    if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
        Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
        Write-Ok 'task removed'
    } else { Write-Ok 'no task found' }
    Stop-GlassMic

    if (Test-Path $installedExe) {
        Write-Step 'Restoring the previous default microphone (if glass-mic left CABLE Output as default)'
        $r = Invoke-GlassMic $installedExe @('--restore-mic')
        if ($r.Output) { Write-Ok ($r.Output.Trim() -split "`r?`n" | Select-Object -Last 1) }
    }

    if (-not $SkipFirewall) {
        Write-Step "Removing firewall rule '$FirewallRuleName'"
        if (Get-NetFirewallRule -DisplayName $FirewallRuleName -ErrorAction SilentlyContinue) {
            Invoke-Elevated "Remove-NetFirewallRule -DisplayName '$FirewallRuleName'"
            Write-Ok 'rule removed'
        } else { Write-Ok 'no rule found' }
    }

    if (-not $SkipTailscale -and (Get-Command tailscale -ErrorAction SilentlyContinue)) {
        Write-Step 'Removing tailscale serve for glass-mic'
        $serve = & tailscale serve status 2>$null | Out-String
        if ($serve -match [regex]::Escape("http://127.0.0.1:$Port")) {
            & tailscale serve --https=443 off | Out-Null
            Write-Ok 'tailscale serve (https 443) turned off'
        } else { Write-Ok 'tailscale serve does not point to glass-mic, left unchanged' }
    }

    Write-Step 'Removing the notification app id'
    Remove-Item 'HKCU:\Software\Classes\AppUserModelId\GlassMic' -Recurse -ErrorAction SilentlyContinue
    Write-Ok 'done'

    Write-Step "Removing $InstallDir"
    if (Test-Path $InstallDir) {
        if ($KeepLogs) {
            Get-ChildItem $InstallDir -Exclude 'glass-mic.log*' | Remove-Item -Recurse -Force
            Write-Ok 'removed (logs kept)'
        } else {
            Remove-Item $InstallDir -Recurse -Force
            Write-Ok 'removed'
        }
    } else { Write-Ok 'not present' }
    Write-Host ''
    Write-Host 'glass-mic is uninstalled. VB-CABLE and Tailscale were left installed.' -ForegroundColor Green
    return
}

# ------------------------------------------------------------------------------------------------
Write-Step 'Locating glass-mic.exe'
if (-not $Exe) {
    $candidates = @(
        (Join-Path $PSScriptRoot 'glass-mic.exe'),
        (Join-Path $PSScriptRoot 'target\release\glass-mic.exe')
    )
    $Exe = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $Exe -or -not (Test-Path $Exe)) {
    throw 'glass-mic.exe not found. Put it next to install.ps1 or pass -Exe <path>.'
}
$Exe = (Resolve-Path $Exe).Path
Write-Ok $Exe

Write-Step 'Checking VB-CABLE'
$devices = (Invoke-GlassMic $Exe @('--list')).Output
if ("$devices" -notmatch 'CABLE Input') {
    Write-Host $devices
    throw 'VB-CABLE is not installed ("CABLE Input" missing). Install it from https://vb-audio.com/Cable/, reboot, then run this script again.'
}
Write-Ok 'CABLE Input found'

Write-Step "Installing to $InstallDir"
Stop-GlassMic
New-Item -ItemType Directory -Force $InstallDir | Out-Null
if ($Exe -ine $installedExe) { Copy-Item $Exe $installedExe -Force }
Write-Ok $installedExe

if (-not $SkipFirewall) {
    Write-Step "Firewall: inbound UDP $RtcPort for glass-mic (WebRTC)"
    $existing = Get-NetFirewallRule -DisplayName $FirewallRuleName -ErrorAction SilentlyContinue
    $ruleOk = $false
    if ($existing) {
        $portFilter = ($existing | Get-NetFirewallPortFilter)
        $appFilter = ($existing | Get-NetFirewallApplicationFilter)
        $ruleOk = ($existing.Enabled -eq 'True') -and ($existing.Direction -eq 'Inbound') -and
            ($existing.Action -eq 'Allow') -and ($portFilter.Protocol -eq 'UDP') -and
            ("$($portFilter.LocalPort)" -eq "$RtcPort") -and ($appFilter.Program -ieq $installedExe)
    }
    if ($ruleOk) {
        Write-Ok 'rule already present'
    } else {
        Write-Note 'Windows asks for admin rights once to add the firewall rule.'
        $cmd = "Remove-NetFirewallRule -DisplayName '$FirewallRuleName' -ErrorAction SilentlyContinue; " +
            "New-NetFirewallRule -DisplayName '$FirewallRuleName' -Direction Inbound -Action Allow " +
            "-Protocol UDP -LocalPort $RtcPort -Program '$installedExe' -Profile Any | Out-Null"
        Invoke-Elevated $cmd
        Write-Ok 'rule added'
    }
}

Write-Step "Scheduled task '$TaskName' (at logon, watchdog every 5 minutes)"
$argList = "--device `"CABLE Input`" --port $Port --rtc-port $RtcPort --log-file `"$logFile`""
if ($ExtraArgs) { $argList = "$argList $ExtraArgs" }
$action = New-ScheduledTaskAction -Execute $installedExe -Argument $argList -WorkingDirectory $InstallDir
$user = "$env:USERDOMAIN\$env:USERNAME"
$triggers = @(
    (New-ScheduledTaskTrigger -AtLogOn -User $user),
    (New-ScheduledTaskTrigger -Once -At (Get-Date).AddMinutes(1) -RepetitionInterval (New-TimeSpan -Minutes 5))
)
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew `
    -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)
$principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited
Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $triggers -Settings $settings `
    -Principal $principal -Description 'glass-mic: iPad/iPhone microphone as a Windows microphone (VB-CABLE)' `
    -Force | Out-Null
Write-Ok 'registered'

$tsName = $null
if (-not $SkipTailscale) {
    Write-Step "Tailscale: https://<this pc>/ -> http://127.0.0.1:$Port"
    if (-not (Get-Command tailscale -ErrorAction SilentlyContinue)) {
        Write-Note 'Tailscale is not installed. Install it (https://tailscale.com/download) on this PC and the iPad, then run this script again.'
    } else {
        $tsName = Get-TailscaleName
        & tailscale serve --bg --https=443 "http://127.0.0.1:$Port" 2>&1 | Out-Null
        if ($LASTEXITCODE -ne 0) {
            Write-Note 'tailscale serve failed. Enable MagicDNS and HTTPS certificates in the Tailscale admin console (DNS page), then run this script again.'
        } else { Write-Ok 'tailscale serve configured' }
    }
}

Write-Step 'Starting glass-mic'
Start-ScheduledTask -TaskName $TaskName
$ok = $false
for ($i = 0; $i -lt 20; $i++) {
    Start-Sleep -Milliseconds 500
    try {
        $stats = Invoke-RestMethod "http://127.0.0.1:$Port/api/stats" -TimeoutSec 2
        $ok = $true; break
    } catch { }
}
if (-not $ok) { throw "glass-mic did not answer on port $Port. See $logFile" }
Write-Ok "running, version $($stats.version), output: $($stats.device)"

Write-Host ''
if ($tsName) {
    Write-Host "Open on the iPad (Safari, Tailscale connected):  https://$tsName/" -ForegroundColor Green
} else {
    Write-Host "glass-mic runs on http://127.0.0.1:$Port/ . The iPad needs HTTPS, see README (Tailscale)." -ForegroundColor Green
}
