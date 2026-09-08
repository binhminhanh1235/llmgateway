param(
    [string]$BaseUrl = $env:LLMGATEWAY_URL,
    [string]$ApiKey = $env:LLMGATEWAY_API_KEY,
    [string[]]$Models = @(),
    [string]$AnthropicVersion = $env:ANTHROPIC_VERSION,
    [string]$AnthropicBeta = $env:ANTHROPIC_BETA,
    [int]$MinToolTurns = 20,
    [int]$MaxModelTurns = 32
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

if ([string]::IsNullOrWhiteSpace($BaseUrl)) {
    $BaseUrl = "http://127.0.0.1:7331"
}
if ([string]::IsNullOrWhiteSpace($ApiKey)) {
    throw "ApiKey is required. Pass -ApiKey or set LLMGATEWAY_API_KEY."
}
if ($Models.Count -eq 0) {
    if ([string]::IsNullOrWhiteSpace($env:CLAUDE_CODE_MODELS)) {
        $Models = @(
            "deepseek-web/deepseek-web-default",
            "gemini-web/gemini-web-flash"
        )
    } else {
        $Models = @(
            $env:CLAUDE_CODE_MODELS -split "\s+" |
                Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
        )
    }
}
if ([string]::IsNullOrWhiteSpace($AnthropicVersion)) {
    $AnthropicVersion = "2023-06-01"
}
if ([string]::IsNullOrWhiteSpace($AnthropicBeta)) {
    $AnthropicBeta = "mid-conversation-output-config-2026-07-01,mid-conversation-system-clear-at-2026-08-21,prompt-caching-2024-07-31"
}

if (-not $PSBoundParameters.ContainsKey("MinToolTurns") -and $env:CLAUDE_CODE_MIN_TOOL_TURNS) {
    $MinToolTurns = [int]$env:CLAUDE_CODE_MIN_TOOL_TURNS
}
if (-not $PSBoundParameters.ContainsKey("MaxModelTurns") -and $env:CLAUDE_CODE_MAX_MODEL_TURNS) {
    $MaxModelTurns = [int]$env:CLAUDE_CODE_MAX_MODEL_TURNS
}

if ($MinToolTurns -lt 20) {
    throw "CLAUDE_CODE_MIN_TOOL_TURNS must be >= 20 for long-horizon acceptance."
}
if ($MaxModelTurns -le $MinToolTurns) {
    throw "CLAUDE_CODE_MAX_MODEL_TURNS must be greater than CLAUDE_CODE_MIN_TOOL_TURNS."
}

$bashRunner = Join-Path $PSScriptRoot "live-claude-code-compat.sh"
if (-not (Test-Path -LiteralPath $bashRunner)) {
    throw "Could not find sibling runner: $bashRunner"
}

$source = [System.IO.File]::ReadAllText($bashRunner)
$match = [regex]::Match(
    $source,
    "(?ms)python3 <<'PY'\r?\n(?<code>.*?)\r?\nPY\s*$"
)
if (-not $match.Success) {
    throw "Could not extract the canonical Python acceptance runner from live-claude-code-compat.sh."
}

$python = $null
$pythonPrefix = @()
foreach ($candidate in @(
    @{ Command = "python"; Prefix = @() },
    @{ Command = "python3"; Prefix = @() },
    @{ Command = "py"; Prefix = @("-3") }
)) {
    $resolved = Get-Command $candidate.Command -ErrorAction SilentlyContinue
    if ($null -ne $resolved) {
        $python = $resolved.Source
        $pythonPrefix = @($candidate.Prefix)
        break
    }
}
if ([string]::IsNullOrWhiteSpace($python)) {
    throw "Python 3 is required. Install Python 3 or make python/python3/py available on PATH."
}

$env:BASE_URL = $BaseUrl.TrimEnd("/")
$env:API_KEY = $ApiKey
$env:MODELS = ($Models -join " ")
$env:ANTHROPIC_VERSION_VALUE = $AnthropicVersion
$env:ANTHROPIC_BETA_VALUE = $AnthropicBeta
$env:MIN_TOOL_TURNS = [string]$MinToolTurns
$env:MAX_MODEL_TURNS = [string]$MaxModelTurns

$tempRunner = Join-Path ([System.IO.Path]::GetTempPath()) ("llmgateway-claude-code-live-" + [guid]::NewGuid().ToString("N") + ".py")

try {
    [System.IO.File]::WriteAllText(
        $tempRunner,
        $match.Groups["code"].Value,
        (New-Object System.Text.UTF8Encoding($false))
    )

    Write-Host "[claude-code-live] Base URL: $($env:BASE_URL)"
    Write-Host "[claude-code-live] Models: $($env:MODELS)"
    Write-Host "[claude-code-live] Min tool turns: $MinToolTurns"
    Write-Host "[claude-code-live] Max model turns: $MaxModelTurns"
    Write-Host "[claude-code-live] Python: $python $($pythonPrefix -join ' ')"

    & $python @pythonPrefix $tempRunner
    $exitCode = $LASTEXITCODE
    if ($exitCode -ne 0) {
        exit $exitCode
    }
} finally {
    Remove-Item -LiteralPath $tempRunner -Force -ErrorAction SilentlyContinue
}
