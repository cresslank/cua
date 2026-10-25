# Disposable GitHub-hosted Windows only. No installation, input injection, or production changes.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$CandidateDirectory,
    [Parameter(Mandatory)][string]$OutputDirectory,
    [Parameter(Mandatory)][ValidatePattern('^[0-9a-f]{40}$')][string]$ExpectedSource,
    [switch]$PolicyFirst,
    [switch]$IncludeLockTimeoutControl
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not $IsWindows -or $env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:GITHUB_REPOSITORY -ne 'cresslank/cua') {
    throw 'Native foreground diagnostics are restricted to disposable GitHub-hosted cresslank/cua Windows runners.'
}
$CandidateDirectory = (Resolve-Path $CandidateDirectory).Path
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
function Save-Json($Path, $Value) { ConvertTo-Json -InputObject $Value -Depth 40 | Set-Content -LiteralPath $Path -Encoding utf8 }

# Recheck the exact extracted bytes before executing any artifact command.
$provenance = Get-Content -Raw -LiteralPath (Join-Path $CandidateDirectory 'provenance.json') | ConvertFrom-Json -AsHashtable
if ($provenance.source_sha -cne $ExpectedSource -or $provenance.runner_os -cne 'Windows') { throw 'Candidate provenance mismatch' }
$names = @('cua-driver.exe', 'cua-cursor-theme.exe', 'cua-driver-uia.exe')
if ((@($provenance.files.Keys | Sort-Object) -join ',') -cne (($names | Sort-Object) -join ',')) { throw 'Unexpected executable inventory' }
foreach ($name in $names) {
    if ((Get-FileHash -LiteralPath (Join-Path $CandidateDirectory $name) -Algorithm SHA256).Hash.ToLowerInvariant() -cne $provenance.files[$name]) {
        throw "Candidate hash mismatch: $name"
    }
}
Save-Json (Join-Path $OutputDirectory 'provenance.json') $provenance
Add-Type -Path (Join-Path $PSScriptRoot 'NativeForeground.cs')
$binary = Join-Path $CandidateDirectory 'cua-driver.exe'
$policyPath = 'SOFTWARE\Policies\Microsoft\Edge'
$policyValue = 'HideFirstRunExperience'
$rows = [Collections.Generic.List[object]]::new()
$cleanupErrors = [Collections.Generic.List[string]]::new()
$fixture = $null
$policyOriginal = $null
$timeoutOriginal = $null
$policyTouched = $false
$timeoutTouched = $false
$activeProcesses = [Collections.Generic.List[object]]::new()

function Read-Policy {
    $key = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey($policyPath)
    try {
        $exists = $null -ne $key -and $key.GetValueNames() -contains $policyValue
        return @{ key_existed = $null -ne $key; value_existed = $exists
            value = $(if ($exists) { $key.GetValue($policyValue, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames) } else { $null })
            kind = $(if ($exists) { $key.GetValueKind($policyValue).ToString() } else { $null }) }
    } finally { if ($null -ne $key) { $key.Dispose() } }
}
function Set-OnboardingPolicy {
    $key = [Microsoft.Win32.Registry]::LocalMachine.CreateSubKey($policyPath)
    try { $key.SetValue($policyValue, 1, [Microsoft.Win32.RegistryValueKind]::DWord) } finally { $key.Dispose() }
    $actual = Read-Policy
    if (-not $actual.value_existed -or $actual.value -ne 1 -or $actual.kind -ne 'DWord') { throw 'Onboarding policy readback failed' }
    return $actual
}
function Restore-Policy {
    if ($policyOriginal.value_existed) {
        $key = [Microsoft.Win32.Registry]::LocalMachine.CreateSubKey($policyPath)
        try { $key.SetValue($policyValue, $policyOriginal.value, [Enum]::Parse([Microsoft.Win32.RegistryValueKind], $policyOriginal.kind)) }
        finally { $key.Dispose() }
    } else {
        $key = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey($policyPath, $true)
        if ($null -ne $key) {
            try { $key.DeleteValue($policyValue, $false); $empty = $key.ValueCount -eq 0 -and $key.SubKeyCount -eq 0 }
            finally { $key.Dispose() }
            if (-not $policyOriginal.key_existed -and $empty) { [Microsoft.Win32.Registry]::LocalMachine.DeleteSubKey($policyPath, $false) }
        }
    }
    $actual = Read-Policy
    Save-Json (Join-Path $OutputDirectory 'policy-restoration.json') $actual
    foreach ($field in @('key_existed', 'value_existed', 'kind', 'value')) {
        if ((ConvertTo-Json -InputObject $actual[$field] -Compress) -cne (ConvertTo-Json -InputObject $policyOriginal[$field] -Compress)) { throw "Onboarding policy restoration mismatch: $field" }
    }
}
function Save-Processes([string]$Path, [string]$Phase) {
    # Query while launch is outstanding as well as before/after; preserve actual arguments and default URL handler.
    $data = @(Get-CimInstance -ClassName Win32_Process -OperationTimeoutSec 2 -Filter "Name = 'msedge.exe' OR Name = 'cua-driver.exe' OR Name = 'explorer.exe' OR Name = 'chrome.exe' OR Name = 'firefox.exe'" |
        Select-Object ProcessId, ParentProcessId, SessionId, CreationDate, Name, ExecutablePath, CommandLine)
    @{ utc = [DateTime]::UtcNow.ToString('O'); phase = $Phase; processes = $data } | ConvertTo-Json -Compress -Depth 10 |
        Add-Content -LiteralPath $Path -Encoding utf8
}
function Stop-DisposableEdge {
    # Explicitly limited by the entry guard; these are not host/user browser processes.
    @(Get-Process -Name msedge -ErrorAction SilentlyContinue) | Stop-Process -Force -ErrorAction Stop
    $deadline = [Diagnostics.Stopwatch]::StartNew()
    while (@(Get-Process -Name msedge -ErrorAction SilentlyContinue).Count -gt 0) {
        if ($deadline.Elapsed.TotalSeconds -gt 5) { throw 'Edge processes survived disposable cleanup' }
        Start-Sleep -Milliseconds 100
    }
}
function Get-LaunchVerdict($Raw, $AtReturn, $After) {
    # Pure classification: isError=true remains a failure even without a PID/structuredContent.
    $result = $Raw['result']
    $driverOk = -not $Raw.ContainsKey('error') -and $null -ne $result -and $result['isError'] -ne $true -and $result.ContainsKey('content')
    $structured = if ($null -ne $result) { $result['structuredContent'] } else { $null }
    $driverOk = $driverOk -and $null -ne $structured -and ($structured['pid'] -is [long] -or $structured['pid'] -is [int]) -and $structured['pid'] -gt 0
    $nativeOk = $AtReturn.fixture_is_foreground -and $AtReturn.fixture_responsive -and $After.fixture_is_foreground -and $After.fixture_responsive
    return @{ driver_ok = $driverOk; native_ok = $nativeOk; status = $(if ($driverOk -and $nativeOk) { 'PASS' } else { 'FAIL' }) }
}
function Invoke-Rpc($Proxy, [int]$Id, [string]$Method, $Params, [string]$Directory, [switch]$SampleDuring) {
    $request = @{ jsonrpc = '2.0'; id = $Id; method = $Method; params = $Params }
    Save-Json (Join-Path $Directory "rpc-$Id-request.json") $request
    $Proxy.Send((ConvertTo-Json -InputObject $request -Depth 20 -Compress))
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $nextSample = 500
    while ($clock.Elapsed.TotalSeconds -lt 30) {
        try { $line = $Proxy.Receive(100) }
        catch [TimeoutException] {
            if ($SampleDuring -and $clock.ElapsedMilliseconds -ge $nextSample) {
                Save-Processes (Join-Path $Directory 'processes.jsonl') 'launch-outstanding'
                $nextSample = $clock.ElapsedMilliseconds + 500
            }
            continue
        }
        $response = ConvertFrom-Json -InputObject $line -AsHashtable
        if ($response.ContainsKey('id') -and $response.id -eq $Id) {
            # Capture the independent HWND/PID oracle before serialization, metadata or further driver calls.
            $nativeAtReturn = $fixture.Snapshot()
            Save-Json (Join-Path $Directory "rpc-$Id-response.json") $response
            return @{ raw = $response; native_at_return = $nativeAtReturn; elapsed_ms = $clock.ElapsedMilliseconds }
        }
    }
    throw "RPC $Method exceeded 30s; launch is not automatically retried"
}

try {
    $policyOriginal = Read-Policy
    $timeoutOriginal = [EdgeForegroundDiagnostic.NativeForeground]::LockTimeout()
    Save-Json (Join-Path $OutputDirectory 'environment.json') @{
        harness_sha = $env:GITHUB_SHA; candidate_source = $ExpectedSource
        runner_os = $env:RUNNER_OS; image_os = $env:ImageOS; image_version = $env:ImageVersion
        powershell = $PSVersionTable.PSVersion.ToString(); os_version = [Environment]::OSVersion.ToString()
        session_id = (Get-Process -Id $PID).SessionId; policy_original = $policyOriginal; foreground_lock_timeout = $timeoutOriginal
        https_user_choice = @(Get-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\Shell\Associations\UrlAssociations\https\UserChoice' -ErrorAction SilentlyContinue | Select-Object ProgId)
        edge_installations = @(Get-Item -LiteralPath 'C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe', 'C:\Program Files\Microsoft\Edge\Application\msedge.exe' -ErrorAction SilentlyContinue |
            Select-Object FullName, @{ Name = 'version'; Expression = { $_.VersionInfo.FileVersion } })
        sfw_visibility = 'Unmodified artifact cannot expose internal driver SetForegroundWindow return values. Harness calls below are explicitly separate controls.'
    }
    $fixture = [EdgeForegroundDiagnostic.NativeForeground]::new((Join-Path $OutputDirectory 'native-timeline.jsonl'))
    $variants = if ($PolicyFirst) { @('hklm-hide-first-run', 'stock-flags') } else { @('stock-flags', 'hklm-hide-first-run') }
    if ($IncludeLockTimeoutControl) { $variants += 'stock-flags-lock-timeout-zero' }
    foreach ($variant in $variants) {
        $directory = Join-Path $OutputDirectory $variant
        New-Item -ItemType Directory -Path $directory | Out-Null
        $row = [ordered]@{ variant = $variant; status = 'BLOCKED'; driver_ok = $false; native_ok = $false; error = $null }
        $daemon = $null; $proxy = $null
        try {
            $fixture.Mark("$variant/setup")
            Stop-DisposableEdge
            if ($variant -eq 'hklm-hide-first-run') {
                $policyTouched = $true
                Save-Json (Join-Path $directory 'policy.json') (Set-OnboardingPolicy)
            } else {
                if ($policyTouched) { Restore-Policy; $policyTouched = $false }
                Save-Json (Join-Path $directory 'policy.json') (Read-Policy)
            }
            if ($variant -eq 'stock-flags-lock-timeout-zero') {
                $fixture.ActivateFixture('timeout-control setup only') | Out-Null
                $timeoutTouched = $true
                $change = $fixture.SetLockTimeout(0)
                Save-Json (Join-Path $directory 'timeout-change.json') $change
                if (-not $change.requested -or $change.readback -ne 0) { throw 'ForegroundLockTimeout=0 was not applied' }
            }
            $state = Join-Path $directory 'driver-state'
            New-Item -ItemType Directory -Path (Join-Path $state '.cua-driver') -Force | Out-Null
            $socket = '\\.\pipe\cua-edge-diagnostic-' + [Guid]::NewGuid().ToString('N')
            $pidFile = Join-Path $directory 'daemon.pid'
            $daemonArgs = @('serve', '--socket', $socket, '--pid-file', $pidFile, '--no-overlay', '--no-permissions-gate')
            $daemon = [EdgeForegroundDiagnostic.LineProcess]::new($binary, $daemonArgs, $state, (Join-Path $directory 'daemon'), $true)
            $activeProcesses.Add($daemon)
            $readyClock = [Diagnostics.Stopwatch]::StartNew()
            while (-not (Test-Path -LiteralPath $pidFile)) {
                if ($daemon.Exited -or $readyClock.Elapsed.TotalSeconds -gt 10) { throw 'Private daemon PID file readiness failed; inspect daemon stderr' }
                Start-Sleep -Milliseconds 50
            }
            if ([int](Get-Content -Raw -LiteralPath $pidFile) -ne $daemon.Pid) { throw 'Daemon PID file does not match owned process' }
            $pipeReady = $false
            while ($readyClock.Elapsed.TotalSeconds -lt 10 -and -not $daemon.Exited) {
                $probe = [EdgeForegroundDiagnostic.LineProcess]::ProbePipe($socket)
                if ($null -ne $probe) {
                    Add-Content -LiteralPath (Join-Path $directory 'readiness.jsonl') -Value $probe -Encoding utf8
                    $probeValue = ConvertFrom-Json -InputObject $probe -AsHashtable
                    if ($probeValue['ok'] -eq $true) { $pipeReady = $true; break }
                }
                Start-Sleep -Milliseconds 50
            }
            if (-not $pipeReady) { throw 'Private daemon did not complete the testkit list protocol readiness exchange' }
            $proxy = [EdgeForegroundDiagnostic.LineProcess]::new($binary, @('mcp', '--socket', $socket), $state, (Join-Path $directory 'proxy'), $false)
            $activeProcesses.Add($proxy)
            $initialized = Invoke-Rpc $proxy 1 'initialize' @{ protocolVersion = '2024-11-05'; capabilities = @{}; clientInfo = @{ name = 'edge-foreground-diagnostic'; version = '1' } } $directory
            if ($initialized.raw.ContainsKey('error') -or -not $initialized.raw.ContainsKey('result')) { throw 'MCP initialization failed' }
            $proxy.Send('{"jsonrpc":"2.0","method":"notifications/initialized"}')
            Save-Json (Join-Path $directory 'ownership.json') @{ daemon_pid = $daemon.Pid; proxy_pid = $proxy.Pid
                fixture_pid = $fixture.Pid; fixture_hwnd = $fixture.Hwnd; daemon_args = $daemonArgs; socket = $socket
                driver_state_root = $state; driver_state_env = @{ HOME = $state; CUA_DRIVER_RS_HOME = (Join-Path $state '.cua-driver') } }
            Save-Processes (Join-Path $directory 'processes.jsonl') 'before-launch'
            $activation = $fixture.ActivateFixture('launch precondition only')
            Save-Json (Join-Path $directory 'precondition-activation.json') $activation
            if (-not $activation.confirmed) { throw 'Owned fixture foreground precondition failed' }
            $before = $fixture.Snapshot()
            if (-not $before.fixture_responsive) { throw 'Owned fixture does not respond to WM_NULL' }
            $grant = $fixture.Grant([uint32]$daemon.Pid)
            Save-Json (Join-Path $directory 'asfw-grant.json') $grant
            if (-not $grant.allowed -or -not $fixture.IsFixtureForeground()) { throw 'Exact-daemon foreground permission precondition failed' }
            $fixture.Mark("$variant/launch")
            $launch = Invoke-Rpc $proxy 2 'tools/call' @{ name = 'launch_app'; arguments = @{
                name = 'Microsoft Edge'; urls = @('https://example.com')
                additional_arguments = @('--no-first-run', '--no-default-browser-check')
            } } $directory -SampleDuring
            $fixture.Mark("$variant/passive-post-oracle")
            Start-Sleep -Milliseconds 500
            $after = $fixture.Snapshot()
            $raw = $launch.raw
            $verdict = Get-LaunchVerdict $raw $launch.native_at_return $after
            $row.driver_ok = $verdict.driver_ok
            $row.native_ok = $verdict.native_ok
            $row.status = $verdict.status
            $row.launch_elapsed_ms = $launch.elapsed_ms
            Save-Json (Join-Path $directory 'oracle-before-diagnostic-controls.json') @{
                raw_response = $raw; native_at_return = $launch.native_at_return; native_after_500ms = $after
                driver_ok = $row.driver_ok; native_ok = $row.native_ok; status = $row.status
            }
            Save-Processes (Join-Path $directory 'processes.jsonl') 'after-launch'
            Save-Json (Join-Path $directory 'native-windows.json') ($fixture.Windows())
            # A separate driver read, never a relaunch; no ambiguous-outcome auto retry.
            $windows = Invoke-Rpc $proxy 3 'tools/call' @{ name = 'list_windows'; arguments = @{} } $directory
            $row.followup_list_windows_ok = -not $windows.raw.ContainsKey('error') -and $null -ne $windows.raw['result'] -and $windows.raw['result']['isError'] -ne $true
            if (-not $row.followup_list_windows_ok) { $row.status = 'FAIL' }
            # This changes foreground ONLY AFTER the immutable result/oracles above. It cannot rescue a FAIL.
            $fixture.Mark("$variant/diagnostic-control-NOT-driver-success")
            Save-Json (Join-Path $directory 'harness-sfw-control-NOT-driver-success.json') ($fixture.ActivateFixture('post-oracle diagnostic; not a driver result'))
        } catch {
            $row.error = $_.ToString()
            if ($row.status -eq 'PASS') { $row.status = 'FAIL' }
            Save-Json (Join-Path $directory 'exception.json') @{ error = $_.ToString(); script_stack = $_.ScriptStackTrace }
            if ($null -ne $fixture) { Save-Json (Join-Path $directory 'exception-native-snapshot.json') ($fixture.Snapshot()) }
        } finally {
            foreach ($owned in @($proxy, $daemon)) {
                if ($null -ne $owned) {
                    try { $owned.Dispose(); $activeProcesses.Remove($owned) | Out-Null }
                    catch { $cleanupErrors.Add("$variant driver cleanup: $_") }
                }
            }
            try { Stop-DisposableEdge } catch { $cleanupErrors.Add("$variant Edge cleanup: $_") }
            Save-Json (Join-Path $directory 'result.json') $row
            $rows.Add($row)
        }
    }
} catch {
    $cleanupErrors.Add("Harness setup/infrastructure: $_")
    Save-Json (Join-Path $OutputDirectory 'harness-exception.json') @{ error = $_.ToString(); script_stack = $_.ScriptStackTrace }
} finally {
    # Independent cleanup attempts: one failure must not prevent policy/timeout restoration.
    foreach ($owned in $activeProcesses) { try { $owned.Dispose() } catch { $cleanupErrors.Add("Remaining process cleanup: $_") } }
    try { Stop-DisposableEdge } catch { $cleanupErrors.Add("Final Edge cleanup: $_") }
    if ($timeoutTouched) {
        try {
            $restored = $fixture.SetLockTimeout($timeoutOriginal)
            Save-Json (Join-Path $OutputDirectory 'timeout-restoration.json') $restored
            if (-not $restored.requested -or $restored.readback -ne $timeoutOriginal) { throw 'ForegroundLockTimeout restoration mismatch' }
        } catch { $cleanupErrors.Add("Timeout restoration: $_") }
    }
    if ($policyTouched) { try { Restore-Policy } catch { $cleanupErrors.Add("Policy restoration: $_") } }
    if ($null -ne $fixture) { try { $fixture.Dispose() } catch { $cleanupErrors.Add("Fixture cleanup: $_") } }
    $expectedCount = if ($IncludeLockTimeoutControl) { 3 } else { 2 }
    $failed = @($rows | Where-Object { $_.status -ne 'PASS' }).Count
    $summary = @{ qualification = 'Scratch diagnostic only, NOT product/release qualification'; candidate_source = $ExpectedSource
        expected_variants = $expectedCount; completed_variants = $rows.Count; failed_or_blocked = $failed
        cleanup_errors = @($cleanupErrors.ToArray()); variants = @($rows.ToArray()) }
    Save-Json (Join-Path $OutputDirectory 'summary.json') $summary
    $markdown = @('# Edge native foreground diagnostic (not qualification)', '', "Candidate: ``$ExpectedSource``", '', '| Variant | Driver | Native HWND/PID oracle | Result |', '|---|---|---|---|')
    foreach ($row in $rows) { $markdown += "| $($row.variant) | $($row.driver_ok) | $($row.native_ok) | $($row.status) |" }
    $markdown += @('', "Completed $($rows.Count)/$expectedCount variants. Failures/blocked: $failed. Cleanup/infrastructure errors: $($cleanupErrors.Count).", '',
        'Raw responses and pre-control oracles are preserved. Harness SetForegroundWindow controls are NOT driver success. Internal artifact SetForegroundWindow return values are not instrumented.')
    $markdown | Set-Content -LiteralPath (Join-Path $OutputDirectory 'summary.md') -Encoding utf8
    if ($env:GITHUB_STEP_SUMMARY) { $markdown | Add-Content -LiteralPath $env:GITHUB_STEP_SUMMARY -Encoding utf8 }
}
if ($failed -gt 0 -or $rows.Count -ne $expectedCount -or $cleanupErrors.Count -gt 0) { exit 1 }
exit 0
