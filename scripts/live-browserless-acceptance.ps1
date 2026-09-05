param(
    [string]$BaseUrl = "http://127.0.0.1:7331",
    [Parameter(Mandatory = $true)]
    [string]$AccountId,
    [string]$ApiKey = $env:LLMGATEWAY_API_KEY,
    [string]$RouteId = "",
    [switch]$KeepThreads,
    [switch]$SkipStream,
    [switch]$TestCancellation
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

if ([string]::IsNullOrWhiteSpace($ApiKey)) {
    throw "ApiKey is required. Pass -ApiKey or set LLMGATEWAY_API_KEY."
}

$BaseUrl = $BaseUrl.TrimEnd("/")
$Headers = @{ Authorization = "Bearer $ApiKey" }
$CreatedThreads = New-Object System.Collections.Generic.List[string]

function Write-Step([string]$Message) {
    Write-Host "[browserless-live] $Message"
}

function Assert-True([bool]$Condition, [string]$Message) {
    if (-not $Condition) {
        throw "ACCEPTANCE FAILED: $Message"
    }
}

function Invoke-Gateway {
    param(
        [Parameter(Mandatory = $true)][string]$Method,
        [Parameter(Mandatory = $true)][string]$Path,
        [object]$Body = $null
    )

    $params = @{
        Method = $Method
        Uri = "$BaseUrl$Path"
        Headers = $Headers
        UseBasicParsing = $true
    }
    if ($null -ne $Body) {
        $params.ContentType = "application/json"
        $params.Body = ($Body | ConvertTo-Json -Depth 20 -Compress)
    }

    try {
        return Invoke-WebRequest @params
    } catch {
        $response = $_.Exception.Response
        if ($null -ne $response) {
            try {
                $reader = New-Object System.IO.StreamReader($response.GetResponseStream())
                $text = $reader.ReadToEnd()
                throw "HTTP request failed: $Method $Path :: $text"
            } catch {
                throw
            }
        }
        throw
    }
}

function Invoke-GatewayJson {
    param(
        [Parameter(Mandatory = $true)][string]$Method,
        [Parameter(Mandatory = $true)][string]$Path,
        [object]$Body = $null
    )
    $response = Invoke-Gateway -Method $Method -Path $Path -Body $Body
    if ([string]::IsNullOrWhiteSpace($response.Content)) {
        return $null
    }
    return $response.Content | ConvertFrom-Json
}

function Get-Runtime {
    return Invoke-GatewayJson -Method GET -Path "/_llmgateway/browser-accounts/$AccountId/runtime"
}

function Get-Affinity([string]$ThreadId) {
    return Invoke-GatewayJson -Method GET -Path "/_llmgateway/threads/$ThreadId/browser-affinity/$AccountId"
}

function New-TestThread([string]$Title) {
    $thread = Invoke-GatewayJson -Method POST -Path "/v1/threads" -Body @{
        title = $Title
        model = $script:ResolvedRouteId
    }
    Assert-True (-not [string]::IsNullOrWhiteSpace([string]$thread.id)) "thread creation returned no id"
    $CreatedThreads.Add([string]$thread.id)
    return [string]$thread.id
}

function Send-ThreadMessage {
    param(
        [Parameter(Mandatory = $true)][string]$ThreadId,
        [Parameter(Mandatory = $true)][string]$Content,
        [bool]$Stream = $false
    )
    $response = Invoke-Gateway -Method POST -Path "/v1/threads/$ThreadId/messages" -Body @{
        content = $Content
        model = $script:ResolvedRouteId
        stream = $Stream
    }
    $routeHeader = [string]$response.Headers["x-llmgateway-route"]
    Assert-True ($routeHeader -eq $script:ResolvedRouteId) "request routed through '$routeHeader' instead of '$script:ResolvedRouteId'"
    return $response
}

function Assert-BrowserClosed([string]$Phase) {
    Start-Sleep -Milliseconds 250
    $runtime = Get-Runtime
    Assert-True (-not [bool]$runtime.browser_running) "Chromium is still running after $Phase"
    return $runtime
}

function Assert-DirectReady([object]$Runtime) {
    Assert-True ([bool]$Runtime.auth_snapshot_available) "saved browser auth snapshot is unavailable"
    Assert-True ([string]$Runtime.adapter.status -eq "ready") "direct adapter is not ready: $($Runtime.adapter.status) :: $($Runtime.adapter.message)"
    Assert-True ([bool]$Runtime.direct_ready) "runtime did not classify the account as direct-ready"
    Assert-True ([string]$Runtime.effective_transport -eq "direct-http") "effective transport is '$($Runtime.effective_transport)', expected direct-http"
    Assert-True (
        [string]$Runtime.adapter.adapter_id -in @("gemini-web-http", "chatgpt-web-http")
    ) "unexpected direct adapter '$($Runtime.adapter.adapter_id)'"
}

function Assert-DirectExecution([object]$Runtime, [string]$Phase) {
    Assert-True ($null -ne $Runtime.last_execution) "no execution transport telemetry after $Phase"
    Assert-True ([string]$Runtime.last_execution.transport -eq "direct-http") "turn used '$($Runtime.last_execution.transport)' after $Phase instead of direct-http"
    Assert-True (-not [bool]$Runtime.last_execution.browser_fallback) "turn used browser fallback after $Phase"
    Assert-True (
        [string]$Runtime.last_execution.adapter_id -in @("gemini-web-http", "chatgpt-web-http")
    ) "turn used unexpected adapter '$($Runtime.last_execution.adapter_id)' after $Phase"
}

function Mapping-Url([object]$Affinity) {
    if ($null -eq $Affinity.mapping) {
        return ""
    }
    return [string]$Affinity.mapping.conversation_url
}

function Assert-AffinityPresent([object]$Affinity, [string]$Phase) {
    Assert-True ($null -ne $Affinity.mapping) "native conversation mapping is missing after $Phase"
    Assert-True (-not [string]::IsNullOrWhiteSpace((Mapping-Url $Affinity))) "native conversation URL is missing after $Phase"
    Assert-True ([int64]$Affinity.mapping.last_synced_ordinal -gt 0) "native mapping is not synced after $Phase"
}

function Cleanup-Threads {
    if ($KeepThreads) {
        Write-Step "Keeping acceptance threads: $($CreatedThreads -join ', ')"
        return
    }
    foreach ($threadId in $CreatedThreads) {
        try {
            $null = Invoke-GatewayJson -Method DELETE -Path "/v1/threads/$threadId"
        } catch {
            Write-Warning "Failed to delete acceptance thread '$threadId': $($_.Exception.Message)"
        }
    }
}

try {
    Write-Step "Checking gateway health"
    $health = Invoke-GatewayJson -Method GET -Path "/_llmgateway/health"
    Assert-True ([string]$health.status -eq "ok") "gateway health is not ok"

    Write-Step "Resolving route for account '$AccountId'"
    if ([string]::IsNullOrWhiteSpace($RouteId)) {
        $models = Invoke-GatewayJson -Method GET -Path "/v1/models"
        $route = @($models.data | Where-Object {
            $_.llmgateway.kind -eq "route" -and $_.llmgateway.account -eq $AccountId
        }) | Select-Object -First 1
        Assert-True ($null -ne $route) "no selectable route was found for account '$AccountId'"
        $script:ResolvedRouteId = [string]$route.id
    } else {
        $script:ResolvedRouteId = $RouteId
    }
    Write-Step "Using route '$script:ResolvedRouteId'"

    Write-Step "Checking direct transport readiness"
    $runtime = Get-Runtime
    if ([bool]$runtime.browser_running) {
        Write-Step "Chromium is currently running; stopping it before browserless acceptance"
        $null = Invoke-GatewayJson -Method POST -Path "/_llmgateway/browser-sessions/$($runtime.session_id)/driver/stop"
        $runtime = Assert-BrowserClosed "initial stop"
    }
    Assert-DirectReady $runtime

    Write-Step "Scenario 1/4: fresh native conversation with Chromium closed"
    $threadA = New-TestThread "Browserless acceptance A $(Get-Date -Format s)"
    $responseA1 = Send-ThreadMessage -ThreadId $threadA -Content "Reply with exactly: browserless-a1"
    $jsonA1 = $responseA1.Content | ConvertFrom-Json
    $assistantA1 = [string]$jsonA1.choices[0].message.content
    Assert-True (-not [string]::IsNullOrWhiteSpace($assistantA1)) "fresh thread returned empty assistant content"
    $runtimeA1 = Assert-BrowserClosed "fresh thread"
    Assert-DirectExecution $runtimeA1 "fresh thread"
    $affinityA1 = Get-Affinity $threadA
    Assert-AffinityPresent $affinityA1 "fresh thread"
    $urlA = Mapping-Url $affinityA1
    $ordinalA1 = [int64]$affinityA1.mapping.last_synced_ordinal

    Write-Step "Scenario 2/4: second turn keeps the same native conversation"
    $responseA2 = Send-ThreadMessage -ThreadId $threadA -Content "Reply with exactly: browserless-a2"
    $jsonA2 = $responseA2.Content | ConvertFrom-Json
    Assert-True (-not [string]::IsNullOrWhiteSpace([string]$jsonA2.choices[0].message.content)) "second turn returned empty assistant content"
    $runtimeA2 = Assert-BrowserClosed "same-thread second turn"
    Assert-DirectExecution $runtimeA2 "same-thread second turn"
    $affinityA2 = Get-Affinity $threadA
    Assert-AffinityPresent $affinityA2 "same-thread second turn"
    Assert-True ((Mapping-Url $affinityA2) -eq $urlA) "same local thread switched native conversation: '$urlA' -> '$(Mapping-Url $affinityA2)'"
    Assert-True ([int64]$affinityA2.mapping.last_synced_ordinal -gt $ordinalA1) "same-thread native sync ordinal did not advance"

    Write-Step "Scenario 3/4: a new local thread gets a different native conversation"
    $threadB = New-TestThread "Browserless acceptance B $(Get-Date -Format s)"
    $responseB1 = Send-ThreadMessage -ThreadId $threadB -Content "Reply with exactly: browserless-b1"
    $jsonB1 = $responseB1.Content | ConvertFrom-Json
    Assert-True (-not [string]::IsNullOrWhiteSpace([string]$jsonB1.choices[0].message.content)) "second local thread returned empty assistant content"
    $runtimeB1 = Assert-BrowserClosed "second local thread"
    Assert-DirectExecution $runtimeB1 "second local thread"
    $affinityB1 = Get-Affinity $threadB
    Assert-AffinityPresent $affinityB1 "second local thread"
    Assert-True ((Mapping-Url $affinityB1) -ne $urlA) "two local threads mapped to the same native conversation"

    if (-not $SkipStream) {
        Write-Step "Scenario 4/4: streaming completes and preserves native affinity"
        $beforeStream = Get-Affinity $threadA
        $beforeStreamOrdinal = [int64]$beforeStream.mapping.last_synced_ordinal
        $streamResponse = Send-ThreadMessage -ThreadId $threadA -Content "Reply with exactly: browserless-stream" -Stream $true
        $sse = [string]$streamResponse.Content
        Assert-True ($sse.Contains("data: [DONE]")) "stream ended without OpenAI [DONE]"
        Assert-True ($sse.Contains('"content"')) "stream returned no assistant content delta"
        Assert-True ($sse -match '"finish_reason"\s*:\s*"[^"]+"') "stream returned no terminal finish_reason"
        $streamFrames = [regex]::Matches($sse, '(?m)^data:\s*\{')
        Assert-True ($streamFrames.Count -ge 2) "stream did not expose incremental SSE frames"
        $runtimeStream = Assert-BrowserClosed "streaming turn"
        Assert-DirectExecution $runtimeStream "streaming turn"
        Start-Sleep -Milliseconds 300
        $afterStream = Get-Affinity $threadA
        Assert-AffinityPresent $afterStream "streaming turn"
        Assert-True ((Mapping-Url $afterStream) -eq $urlA) "streaming turn changed native conversation"
        Assert-True ([int64]$afterStream.mapping.last_synced_ordinal -gt $beforeStreamOrdinal) "streaming native sync ordinal did not advance"
        $threadAfterStream = Invoke-GatewayJson -Method GET -Path "/v1/threads/$threadA"
        $emptyStreamAssistants = @($threadAfterStream.messages | Where-Object {
            $_.role -eq "assistant" -and [string]::IsNullOrWhiteSpace([string]$_.content)
        })
        Assert-True ($emptyStreamAssistants.Count -eq 0) "streaming persisted an empty/stale assistant message"
    }

    if ($TestCancellation) {
        Write-Step "Optional cancellation scenario: aborting a streaming request"
        $threadC = New-TestThread "Browserless cancellation $(Get-Date -Format s)"
        $bodyPath = [System.IO.Path]::GetTempFileName()
        try {
            $cancelBody = @{
                content = "Write a detailed response with at least 800 words. Begin with the word cancellation-test."
                model = $script:ResolvedRouteId
                stream = $true
            } | ConvertTo-Json -Compress
            [System.IO.File]::WriteAllText(
                $bodyPath,
                $cancelBody,
                (New-Object System.Text.UTF8Encoding($false))
            )

            $curlArgs = @(
                "--silent", "--show-error", "--no-buffer",
                "--max-time", "1",
                "-H", "Authorization: Bearer $ApiKey",
                "-H", "Content-Type: application/json",
                "--data-binary", "@$bodyPath",
                "$BaseUrl/v1/threads/$threadC/messages"
            )
            & curl.exe @curlArgs *> $null
            $curlExit = $LASTEXITCODE
            if ($curlExit -eq 28) {
                Write-Step "Streaming request was cancelled by client timeout as expected"
                Start-Sleep -Seconds 2
                $null = Assert-BrowserClosed "cancelled stream"
                $threadDetail = Invoke-GatewayJson -Method GET -Path "/v1/threads/$threadC"
                $persistedAssistants = @($threadDetail.messages | Where-Object {
                    $_.role -eq "assistant"
                })
                Assert-True ($persistedAssistants.Count -eq 0) "cancelled stream persisted an assistant message"
            } else {
                Write-Warning "Cancellation request finished before timeout (curl exit $curlExit); cancellation cleanup was not exercised."
            }
        } finally {
            Remove-Item -LiteralPath $bodyPath -Force -ErrorAction SilentlyContinue
        }
    }

    $finalRuntime = Get-Runtime
    Assert-DirectReady $finalRuntime
    Assert-True (-not [bool]$finalRuntime.browser_running) "Chromium is running at the end of acceptance"

    Write-Host ""
    Write-Host "BROWSERLESS LIVE ACCEPTANCE: PASS" -ForegroundColor Green
    Write-Host "Account: $AccountId"
    Write-Host "Route:   $script:ResolvedRouteId"
    Write-Host "Adapter: $($finalRuntime.adapter.adapter_id)"
    Write-Host "Thread A native: $urlA"
    Write-Host "Thread B native: $(Mapping-Url $affinityB1)"
    Write-Host "Chromium running: $($finalRuntime.browser_running)"
} finally {
    Cleanup-Threads
}
