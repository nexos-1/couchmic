<#
.SYNOPSIS
  Installs, updates or removes CouchMic for the current Windows user.

.DESCRIPTION
  Install / update (default):
    1. Checks that VB-CABLE is installed ("CABLE Input" output device).
    2. Copies couchmic.exe to %LOCALAPPDATA%\CouchMic (stops a running copy first).
    3. Allows inbound UDP for WebRTC in the Windows firewall, from Tailscale addresses only
       (one UAC prompt, only if the rule is missing or different).
    4. Registers a scheduled task: start at logon, plus a 5-minute watchdog that restarts
       CouchMic if it is not running. No admin rights needed.
    5. Runs `tailscale serve` so the iPad reaches https://<pc>.<tailnet>.ts.net/. Refuses if
       that address is already used for something else or Tailscale Funnel is on for it.
    6. Starts CouchMic and checks that it answers.

  -Uninstall reverses all of it, restores the previous default microphone and removes only
  CouchMic's own files.

  Re-running the script is safe; it updates an existing installation in place.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File .\install.ps1

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File .\install.ps1 -Uninstall
#>
[CmdletBinding()]
param(
    # couchmic.exe to install. Default: next to this script, else target\release\couchmic.exe.
    [string]$Exe,
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'CouchMic'),
    [string]$TaskName = 'CouchMic',
    [int]$Port = 8321,
    [int]$RtcPort = 8322,
    # HTTPS port of `tailscale serve` (443, 8443 or 10000).
    [int]$HttpsPort = 443,
    [string]$FirewallRuleName = 'CouchMic (WebRTC UDP)',
    # Extra arguments for CouchMic, e.g. '--no-toast'.
    [string]$ExtraArgs = '',
    [switch]$SkipFirewall,
    [switch]$SkipTailscale,
    [switch]$Uninstall,
    # With -Uninstall: keep the log files.
    [switch]$KeepLogs,
    # Override the safety checks (unusual install folder, running as admin, existing
    # tailscale serve config or Funnel on the HTTPS port).
    [switch]$Force
)

Set-StrictMode -Version 2
$ErrorActionPreference = 'Stop'

function Write-Step([string]$text) { Write-Host "==> $text" -ForegroundColor Cyan }
function Write-Ok([string]$text) { Write-Host "    $text" -ForegroundColor Green }
function Write-Note([string]$text) { Write-Host "    $text" -ForegroundColor Yellow }

# Files CouchMic creates; uninstall removes exactly these, nothing else. The program files live
# in the install folder, the state always in %LOCALAPPDATA%\CouchMic (the data folder).
$OwnFiles = @('couchmic.exe', 'couchmic.log', 'couchmic.log.1', '.couchmic-install')
$OwnDataFiles = @('previous-mic.txt', 'toast-icon.png')
$MarkerName = '.couchmic-install'
# Tailscale address ranges (CGNAT IPv4 and the Tailscale ULA IPv6 prefix).
$TailnetRanges = @('100.64.0.0/10', 'fd7a:115c:a1e0::/48')
$FirewallDescription = 'CouchMic WebRTC audio, tailnet addresses only (managed by install.ps1)'

# ------------------------------------------------------------------------------------------------
# Safety checks

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if ($isAdmin -and -not $Force) {
    throw 'Do not run this script as administrator: it installs for the current user (task, folder, microphone). Run it from a normal PowerShell window; it asks for admin rights itself only for the firewall rule. Use -Force to override.'
}

$InstallDir = [IO.Path]::GetFullPath($InstallDir).TrimEnd('\')
$forbidden = @(
    [IO.Path]::GetPathRoot($InstallDir).TrimEnd('\'),
    $env:USERPROFILE, $env:LOCALAPPDATA, $env:APPDATA, $env:ProgramFiles, ${env:ProgramFiles(x86)},
    $env:SystemRoot, $env:TEMP
) | Where-Object { $_ } | ForEach-Object { [IO.Path]::GetFullPath($_).TrimEnd('\') }
if ($forbidden -contains $InstallDir) {
    throw "Refusing to use '$InstallDir' as the install folder."
}
if ((Split-Path $InstallDir -Leaf) -ne 'CouchMic' -and -not $Force) {
    throw "The install folder should be named 'CouchMic' (got '$InstallDir'). Use -Force to override."
}

$installedExe = Join-Path $InstallDir 'couchmic.exe'
$logFile = Join-Path $InstallDir 'couchmic.log'
$DataDir = Join-Path $env:LOCALAPPDATA 'CouchMic'
$ownUrl = "http://127.0.0.1:$Port"

# ------------------------------------------------------------------------------------------------
# Helpers

function Stop-CouchMic {
    $procs = @(Get-CimInstance Win32_Process -Filter "Name='couchmic.exe'" |
        Where-Object { $_.ExecutablePath -and ([IO.Path]::GetFullPath($_.ExecutablePath) -ieq $installedExe) })
    foreach ($p in $procs) {
        Stop-Process -Id $p.ProcessId -Force -Confirm:$false -ErrorAction SilentlyContinue
        Write-Ok "stopped running CouchMic (pid $($p.ProcessId))"
    }
    if ($procs.Count -gt 0) { Start-Sleep -Milliseconds 800 }
}

# CouchMic is a GUI-subsystem program; `& couchmic.exe` does not reliably hand its output to
# PowerShell 5.1. Redirecting into a file with Start-Process always works.
function Invoke-CouchMic([string]$path, [string[]]$arguments) {
    $out = [IO.Path]::GetTempFileName()
    try {
        $p = Start-Process -FilePath $path -ArgumentList $arguments -RedirectStandardOutput $out `
            -Wait -PassThru -NoNewWindow
        return [pscustomobject]@{ ExitCode = $p.ExitCode; Output = (Get-Content $out -Raw) }
    } finally {
        Remove-Item $out -ErrorAction SilentlyContinue
    }
}

# Single-quoted PowerShell literal, safe for any content (also typographic quotes).
function ConvertTo-PsLiteral([string]$value) {
    return "'" + [System.Management.Automation.Language.CodeGeneration]::EscapeSingleQuotedStringContent($value) + "'"
}

function Invoke-Elevated([string]$command) {
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($command))
    $ps = Join-Path $PSHOME 'powershell.exe'
    if ($isAdmin) {
        & $ps -NoProfile -NonInteractive -EncodedCommand $encoded
        if ($LASTEXITCODE -ne 0) { throw "command failed with exit code $LASTEXITCODE" }
        return
    }
    $p = Start-Process $ps -Verb RunAs -Wait -PassThru -WindowStyle Hidden `
        -ArgumentList '-NoProfile', '-NonInteractive', '-EncodedCommand', $encoded
    if ($p.ExitCode -ne 0) { throw "elevated command failed with exit code $($p.ExitCode)" }
}

# Exact-name firewall lookup (DisplayName accepts wildcards otherwise).
function Get-OwnFirewallRule {
    $name = [Management.Automation.WildcardPattern]::Escape($FirewallRuleName)
    return @(Get-NetFirewallRule -DisplayName $name -ErrorAction SilentlyContinue)
}

function Get-TailscaleExe {
    $candidate = Join-Path $env:ProgramFiles 'Tailscale\tailscale.exe'
    if (Test-Path $candidate) { return $candidate }
    $cmd = Get-Command tailscale.exe -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($cmd) { return $cmd.Source }
    return $null
}

# Runs tailscale without letting stderr output abort the script (PowerShell 5.1 turns
# redirected stderr into terminating errors under ErrorActionPreference=Stop). With -StdoutOnly
# stderr is dropped, so warnings cannot corrupt --json output.
function Invoke-Tailscale([string[]]$arguments, [switch]$StdoutOnly) {
    $ts = Get-TailscaleExe
    if (-not $ts) { return [pscustomobject]@{ ExitCode = -1; Output = 'tailscale not found' } }
    $old = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        if ($StdoutOnly) {
            $out = & $ts @arguments 2>$null | ForEach-Object { "$_" }
        } else {
            $out = & $ts @arguments 2>&1 | ForEach-Object { "$_" }
        }
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $old
    }
    return [pscustomobject]@{ ExitCode = $code; Output = ($out -join "`n") }
}

function Get-TailscaleName {
    $r = Invoke-Tailscale @('status', '--json') -StdoutOnly
    if ($r.ExitCode -ne 0) { return $null }
    try { return ((($r.Output | ConvertFrom-Json).Self.DNSName)).TrimEnd('.') } catch { return $null }
}

# Current `tailscale serve` state for our host and HTTPS port.
function Get-ServeState([string]$dns) {
    $state = [pscustomobject]@{ Root = $null; Funnel = $false; TcpForward = $false; ForwardsToUs = $false; Ok = $false }
    $r = Invoke-Tailscale @('serve', 'status', '--json') -StdoutOnly
    if ($r.ExitCode -ne 0) { return $state }
    if (-not $r.Output.Trim()) { $state.Ok = $true; return $state }
    try { $cfg = $r.Output | ConvertFrom-Json } catch { return $state }
    $state.Ok = $true
    $hostPort = "${dns}:$HttpsPort"
    if ($cfg.PSObject.Properties['Web'] -and $cfg.Web.PSObject.Properties[$hostPort]) {
        $handlers = $cfg.Web.$hostPort.Handlers
        if ($handlers -and $handlers.PSObject.Properties['/']) {
            $state.Root = $handlers.'/'
        }
    }
    if ($cfg.PSObject.Properties['AllowFunnel'] -and $cfg.AllowFunnel.PSObject.Properties[$hostPort]) {
        $state.Funnel = [bool]$cfg.AllowFunnel.$hostPort
    }
    if ($cfg.PSObject.Properties['TCP']) {
        foreach ($prop in $cfg.TCP.PSObject.Properties) {
            $tcp = $prop.Value
            $fwd = if ($tcp -and $tcp.PSObject.Properties['TCPForward']) { "$($tcp.TCPForward)" } else { '' }
            if ($prop.Name -eq "$HttpsPort" -and $fwd) { $state.TcpForward = $true }
            # A raw TCP forward to CouchMic would bypass the Host and Tailscale-user checks:
            # the connection arrives as a plain local one.
            if ($fwd -match "^(127\.0\.0\.1|localhost|\[::1\]):$Port$") { $state.ForwardsToUs = $true }
        }
    }
    return $state
}

function Test-OwnRoot($root) {
    return $root -and $root.PSObject.Properties['Proxy'] -and ($root.Proxy -eq $ownUrl)
}

# ------------------------------------------------------------------------------------------------
if ($Uninstall) {
    Write-Step "Stopping and removing the scheduled task '$TaskName'"
    if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
        Disable-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue | Out-Null
        Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
        Write-Ok 'task removed'
    } else { Write-Ok 'no task found' }
    Stop-CouchMic

    if (Test-Path $installedExe) {
        Write-Step 'Restoring the previous default microphone (if CouchMic left CABLE Output as default)'
        $r = Invoke-CouchMic $installedExe @('--restore-mic')
        if ($r.Output) { Write-Ok ($r.Output.Trim() -split "`r?`n" | Select-Object -Last 1) }
    }

    if (-not $SkipFirewall) {
        Write-Step "Removing firewall rule '$FirewallRuleName'"
        if ((Get-OwnFirewallRule).Count -gt 0) {
            Invoke-Elevated "Get-NetFirewallRule -DisplayName $(ConvertTo-PsLiteral ([Management.Automation.WildcardPattern]::Escape($FirewallRuleName))) | Remove-NetFirewallRule"
            Write-Ok 'rule removed'
        } else { Write-Ok 'no rule found' }
    }

    if (-not $SkipTailscale -and (Get-TailscaleExe)) {
        Write-Step "Removing CouchMic from tailscale serve (https port $HttpsPort)"
        $dns = Get-TailscaleName
        $state = if ($dns) { Get-ServeState $dns } else { $null }
        if ($state -and (Test-OwnRoot $state.Root)) {
            # Only our own mount "/", never other handlers on the port.
            $r = Invoke-Tailscale @('serve', "--https=$HttpsPort", '--set-path=/', 'off')
            if ($r.ExitCode -eq 0) { Write-Ok 'removed' } else { Write-Note "tailscale serve off failed: $($r.Output)" }
        } else { Write-Ok 'CouchMic is not configured there, left unchanged' }
    }

    Write-Step 'Removing the notification app id and settings'
    Remove-Item 'HKCU:\Software\Classes\AppUserModelId\CouchMic' -Recurse -ErrorAction SilentlyContinue
    Remove-Item 'HKCU:\Software\CouchMic' -Recurse -ErrorAction SilentlyContinue
    foreach ($f in $OwnDataFiles) {
        $p = Join-Path $DataDir $f
        if (Test-Path -LiteralPath $p -PathType Leaf) { Remove-Item -LiteralPath $p -Force }
    }
    Write-Ok 'done'

    Write-Step "Removing CouchMic's files from $InstallDir"
    if (Test-Path $InstallDir) {
        foreach ($f in $OwnFiles) {
            if ($KeepLogs -and $f -like 'couchmic.log*') { continue }
            $p = Join-Path $InstallDir $f
            if (Test-Path -LiteralPath $p -PathType Leaf) { Remove-Item -LiteralPath $p -Force }
        }
        if (@(Get-ChildItem -LiteralPath $InstallDir -Force).Count -eq 0) {
            Remove-Item -LiteralPath $InstallDir -Force
            Write-Ok 'folder removed'
        } else {
            Write-Ok 'CouchMic files removed; other files in the folder were left untouched'
        }
    } else { Write-Ok 'not present' }
    if (($DataDir -ine $InstallDir) -and (Test-Path -LiteralPath $DataDir) -and
        @(Get-ChildItem -LiteralPath $DataDir -Force).Count -eq 0) {
        Remove-Item -LiteralPath $DataDir -Force
    }
    Write-Host ''
    Write-Host 'CouchMic is uninstalled. VB-CABLE and Tailscale were left installed.' -ForegroundColor Green
    return
}

# ------------------------------------------------------------------------------------------------
Write-Step 'Locating couchmic.exe'
if (-not $Exe) {
    $candidates = @(
        (Join-Path $PSScriptRoot 'couchmic.exe'),
        (Join-Path $PSScriptRoot 'target\release\couchmic.exe')
    )
    $Exe = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $Exe -or -not (Test-Path $Exe)) {
    throw 'couchmic.exe not found. Put it next to install.ps1 or pass -Exe <path>.'
}
$Exe = (Resolve-Path $Exe).Path
Write-Ok $Exe

Write-Step 'Checking VB-CABLE'
$devices = (Invoke-CouchMic $Exe @('--list')).Output
if ("$devices" -notmatch 'CABLE Input') {
    Write-Host $devices
    throw 'VB-CABLE is not installed ("CABLE Input" missing). Install it from https://vb-audio.com/Cable/, reboot, then run this script again.'
}
Write-Ok 'CABLE Input found'

# Check tailscale serve before changing anything, so a conflict aborts cleanly.
$tsName = $null
$serveNeeded = $false
if (-not $SkipTailscale) {
    Write-Step "Checking tailscale serve (https port $HttpsPort)"
    if (-not (Get-TailscaleExe)) {
        Write-Note 'Tailscale is not installed. Install it (https://tailscale.com/download) on this PC and the iPad, then run this script again.'
    } else {
        $tsName = Get-TailscaleName
        if (-not $tsName) {
            Write-Note 'Tailscale is installed but not connected. Sign in, then run this script again.'
        } else {
            $state = Get-ServeState $tsName
            if ($state.Funnel -and -not $Force) {
                throw "Tailscale Funnel is ON for ${tsName}:$HttpsPort, which would make CouchMic reachable from the internet (CouchMic rejects Funnel requests, but do not rely on that). Turn Funnel off for this port (tailscale funnel --https=$HttpsPort off) or use another -HttpsPort."
            }
            if (-not $state.Ok) {
                throw 'Cannot read the tailscale serve configuration (tailscale serve status --json). Run this script again once Tailscale works, or use -SkipTailscale.'
            }
            if ($state.ForwardsToUs) {
                throw "tailscale serve has a raw TCP forward to 127.0.0.1:$Port. That would bypass CouchMic's access checks; remove it first (tailscale serve status)."
            }
            if ($state.TcpForward -and -not $Force) {
                throw "Port $HttpsPort of $tsName is already used as a TCP forward in tailscale serve. Use another -HttpsPort or -Force."
            }
            if ($state.Root -and -not (Test-OwnRoot $state.Root) -and -not $Force) {
                throw "https://${tsName}:$HttpsPort/ already serves something else in tailscale serve. CouchMic will not replace it. Use another -HttpsPort or -Force."
            }
            $serveNeeded = -not (Test-OwnRoot $state.Root)
            Write-Ok ($(if ($serveNeeded) { 'free, will be configured' } else { 'already points to CouchMic' }))
        }
    }
}

if (-not $SkipFirewall) {
    Write-Step "Firewall: inbound UDP $RtcPort for CouchMic, only on the Tailscale interface from Tailscale addresses"
    $tsAdapter = Get-NetAdapter -IncludeHidden -ErrorAction SilentlyContinue |
        Where-Object { $_.InterfaceDescription -like 'Tailscale*' } | Select-Object -First 1
    $alias = if ($tsAdapter) { $tsAdapter.Name } else { $null }
    if (-not $alias) { Write-Note 'Tailscale adapter not found; the rule is limited to Tailscale addresses only.' }
    $existing = Get-OwnFirewallRule
    $ruleOk = $false
    if ($existing.Count -eq 1) {
        $r0 = $existing[0]
        $portFilter = ($r0 | Get-NetFirewallPortFilter)
        $appFilter = ($r0 | Get-NetFirewallApplicationFilter)
        $addrFilter = ($r0 | Get-NetFirewallAddressFilter)
        $ifFilter = ($r0 | Get-NetFirewallInterfaceFilter)
        $remote = @($addrFilter.RemoteAddress)
        $addrOk = ($remote.Count -eq 2) -and
            (@($remote | Where-Object { $_ -match '^100\.64\.0\.0/(10|255\.192\.0\.0)$' }).Count -eq 1) -and
            (@($remote | Where-Object { $_ -ieq 'fd7a:115c:a1e0::/48' }).Count -eq 1)
        $ifOk = if ($alias) { @($ifFilter.InterfaceAlias) -contains $alias } else { $true }
        $ruleOk = ($r0.Enabled -eq 'True') -and ($r0.Direction -eq 'Inbound') -and
            ($r0.Action -eq 'Allow') -and ($portFilter.Protocol -eq 'UDP') -and
            ("$($portFilter.LocalPort)" -eq "$RtcPort") -and ($appFilter.Program -ieq $installedExe) -and
            $addrOk -and $ifOk
    }
    if ($ruleOk) {
        Write-Ok 'rule already present'
    } else {
        Write-Note 'Windows asks for admin rights once to set the firewall rule.'
        $nameLit = ConvertTo-PsLiteral $FirewallRuleName
        $nameEsc = ConvertTo-PsLiteral ([Management.Automation.WildcardPattern]::Escape($FirewallRuleName))
        $ifArg = if ($alias) { "-InterfaceAlias $(ConvertTo-PsLiteral $alias) " } else { '' }
        $cmd = "`$ErrorActionPreference = 'Stop'; " +
            "Get-NetFirewallRule -DisplayName $nameEsc -ErrorAction SilentlyContinue | Remove-NetFirewallRule; " +
            "New-NetFirewallRule -DisplayName $nameLit -Description $(ConvertTo-PsLiteral $FirewallDescription) " +
            "-Direction Inbound -Action Allow -Protocol UDP -LocalPort $RtcPort " +
            "-RemoteAddress $(($TailnetRanges | ForEach-Object { ConvertTo-PsLiteral $_ }) -join ',') " +
            $ifArg + "-Program $(ConvertTo-PsLiteral $installedExe) -Profile Any | Out-Null"
        # Before anything else is changed: declining the UAC prompt leaves the old install intact.
        Invoke-Elevated $cmd
        Write-Ok 'rule set'
    }
}

Write-Step "Installing to $InstallDir"
$disabledTask = $false
if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
    # Keep the watchdog from restarting the old exe while it is being replaced. The finally block
    # below turns it back on even if a later step fails.
    Disable-ScheduledTask -TaskName $TaskName | Out-Null
    $disabledTask = $true
}
try {
Stop-CouchMic
New-Item -ItemType Directory -Force $InstallDir | Out-Null
if ($Exe -ine $installedExe) { Copy-Item -LiteralPath $Exe -Destination $installedExe -Force }
Set-Content -LiteralPath (Join-Path $InstallDir $MarkerName) -Value 'CouchMic install folder (used by install.ps1 -Uninstall)' -Encoding ascii
Write-Ok $installedExe

Write-Step "Scheduled task '$TaskName' (at logon, watchdog every 5 minutes)"
$argList = "--watchdog --device `"CABLE Input`" --port $Port --rtc-port $RtcPort --log-file `"$logFile`""
if ($tsName -and $HttpsPort -ne 443) { $argList = "$argList --allow-origin https://${tsName}:$HttpsPort" }
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
    -Principal $principal -Description 'CouchMic: iPad/iPhone microphone as a Windows microphone (VB-CABLE)' `
    -Force | Out-Null
Write-Ok 'registered'

if ($serveNeeded) {
    Write-Step "Tailscale: https://${tsName}:$HttpsPort/ -> $ownUrl"
    $r = Invoke-Tailscale @('serve', '--bg', "--https=$HttpsPort", $ownUrl)
    if ($r.Output) { $r.Output -split "`n" | ForEach-Object { if ($_.Trim()) { Write-Host "    $_" } } }
    $after = Get-ServeState $tsName
    if ($r.ExitCode -ne 0 -or -not (Test-OwnRoot $after.Root)) {
        Write-Note 'tailscale serve is not configured. Enable MagicDNS and HTTPS certificates in the Tailscale admin console (DNS page), then run this script again.'
    } else { Write-Ok 'configured (tailnet only)' }
}

Write-Step 'Starting CouchMic'
# A tray "Quit" earlier in this session would keep the watchdog start from running.
Remove-Item 'HKCU:\Software\CouchMic\StoppedByUser' -ErrorAction SilentlyContinue
Start-ScheduledTask -TaskName $TaskName
$ok = $false
for ($i = 0; $i -lt 20; $i++) {
    Start-Sleep -Milliseconds 500
    try {
        $stats = Invoke-RestMethod "$ownUrl/api/stats" -TimeoutSec 2
        $proc = Get-CimInstance Win32_Process -Filter "ProcessId=$($stats.pid)"
        if ($proc -and $proc.ExecutablePath -and ([IO.Path]::GetFullPath($proc.ExecutablePath) -ieq $installedExe)) {
            $ok = $true; break
        }
    } catch { }
}
if (-not $ok) {
    throw "The installed CouchMic did not answer on port $Port (another program may use it). See $logFile"
}
Write-Ok "running, version $($stats.version), output: $($stats.device)"
$disabledTask = $false
} finally {
    if ($disabledTask -and (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue)) {
        # Something failed after the task was disabled: turn it back on and start it, so the
        # user is not left without CouchMic (and the microphone recovery runs).
        Enable-ScheduledTask -TaskName $TaskName | Out-Null
        Start-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
        Write-Note 'install failed; the previous scheduled task was re-enabled and started'
    }
}

Write-Host ''
if ($tsName) {
    $url = if ($HttpsPort -eq 443) { "https://$tsName/" } else { "https://${tsName}:$HttpsPort/" }
    Write-Host "Open on the iPad (Safari, Tailscale connected):  $url" -ForegroundColor Green
} else {
    Write-Host "CouchMic runs on $ownUrl/ . The iPad needs HTTPS, see README (Tailscale)." -ForegroundColor Green
}
