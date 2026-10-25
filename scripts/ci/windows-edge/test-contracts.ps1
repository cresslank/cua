# Portable checks only: no Windows API invocation, desktop, driver execution, or registry writes.
[CmdletBinding()]
param()
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$tokens = $null; $errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile((Join-Path $PSScriptRoot 'diagnose.ps1'), [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw ($errors | Out-String) }
Add-Type -Path (Join-Path $PSScriptRoot 'NativeForeground.cs')
$definition = $ast.Find({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Get-LaunchVerdict' }, $true)
if ($null -eq $definition) { throw 'Missing actual verdict function' }
Invoke-Expression $definition.Extent.Text
$good = [pscustomobject]@{ fixture_is_foreground = $true; fixture_responsive = $true }
$displaced = [pscustomobject]@{ fixture_is_foreground = $false; fixture_responsive = $true }
$hung = [pscustomobject]@{ fixture_is_foreground = $true; fixture_responsive = $false }
$success = @{ result = @{ content = @(); structuredContent = @{ pid = 123 } } }
$cases = @(
    @{ label = 'valid result and exact native oracle'; raw = $success; at = $good; after = $good; expected = 'PASS' },
    @{ label = 'isError preserved despite good native oracle'; raw = @{ result = @{ isError = $true; content = @() } }; at = $good; after = $good; expected = 'FAIL' },
    @{ label = 'isError preserved even with a returned PID'; raw = @{ result = @{ isError = $true; content = @(); structuredContent = @{ pid = 123 } } }; at = $good; after = $good; expected = 'FAIL' },
    @{ label = 'boolean PID rejected'; raw = @{ result = @{ content = @(); structuredContent = @{ pid = $true } } }; at = $good; after = $good; expected = 'FAIL' },
    @{ label = 'string PID rejected'; raw = @{ result = @{ content = @(); structuredContent = @{ pid = '123' } } }; at = $good; after = $good; expected = 'FAIL' },
    @{ label = 'protocol error'; raw = @{ error = @{ code = -32000 } }; at = $good; after = $good; expected = 'FAIL' },
    @{ label = 'missing PID'; raw = @{ result = @{ content = @() } }; at = $good; after = $good; expected = 'FAIL' },
    @{ label = 'zero PID'; raw = @{ result = @{ content = @(); structuredContent = @{ pid = 0 } } }; at = $good; after = $good; expected = 'FAIL' },
    @{ label = 'displaced at tool return'; raw = $success; at = $displaced; after = $good; expected = 'FAIL' },
    @{ label = 'late self activation'; raw = $success; at = $good; after = $displaced; expected = 'FAIL' },
    @{ label = 'hung fixture'; raw = $success; at = $hung; after = $good; expected = 'FAIL' }
)
foreach ($case in $cases) {
    $verdict = Get-LaunchVerdict $case.raw $case.at $case.after
    if ($verdict.status -cne $case.expected) { throw "Verdict failed: $($case.label)" }
    Write-Output "PASS pure verdict: $($case.label)"
}
# Real subprocess/stdio exercise, using PowerShell itself, not a simulated driver result.
$root = Join-Path ([IO.Path]::GetTempPath()) ('edge-transport-check-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $root | Out-Null
$child = $null
try {
    $echoScript = Join-Path $root 'stdio-child.ps1'
    @'
$line = [Console]::ReadLine()
[Console]::WriteLine($line)
[Console]::Error.WriteLine('transport-check-stderr')
'@ | Set-Content -LiteralPath $echoScript -Encoding utf8
    $exe = (Get-Process -Id $PID).Path
    $child = [EdgeForegroundDiagnostic.LineProcess]::new($exe, @('-NoLogo', '-NoProfile', '-File', $echoScript), $root, (Join-Path $root 'transport'), $false)
    $timedOut = $false
    try { $child.Receive(20) | Out-Null } catch [TimeoutException] { $timedOut = $true }
    if (-not $timedOut) { throw 'Real stdio deadline failed to fire' }
    $child.Send('actual-stdio-test')
    if ($child.Receive(10000) -cne 'actual-stdio-test') { throw 'Actual child stdio roundtrip failed' }
    $child.Dispose(); $child = $null
    if ((Get-Content -Raw -LiteralPath (Join-Path $root 'transport.stderr.log')) -notmatch 'transport-check-stderr') { throw 'stderr not drained' }
    Write-Output 'PASS real subprocess stdio roundtrip, stderr drain and bounded cleanup'
} finally {
    if ($null -ne $child) { $child.Dispose() }
    Remove-Item -LiteralPath $root -Recurse -Force
}
Write-Output 'PASS PowerShell parser + C# compilation. These checks do NOT qualify Windows native foreground behavior.'
