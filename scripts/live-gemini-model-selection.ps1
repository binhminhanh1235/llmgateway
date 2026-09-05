param(
    [string]$BaseUrl = "http://127.0.0.1:7331",
    [Parameter(Mandatory = $true)]
    [string]$AccountId,
    [string]$ApiKey = $env:LLMGATEWAY_API_KEY,
    [string]$ModelA = "",
    [string]$ModelB = "",
    [switch]$KeepThreads
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

if ([string]::IsNullOrWhiteSpace($ApiKey)) {
    throw "ApiKey is required. Pass -ApiKey or set LLMGATEWAY_API_KEY."
}

$BaseUrl = $BaseUrl.TrimEnd("/")
$Headers = @{ Authorization = "Bearer $ApiKey" }
$CreatedThreads = New-Object System.Collections.Generic.List[string]

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
    return Invoke-WebRequest @params
}

function Invoke-GatewayJson {
    param(
        [Parameter(Mandatory = $true)][string]$Method,
        [Parameter(Mandatory = $true)][string]$Path,
        [object]$Body = $null
    )
    $response = Invoke-Gateway -Method $Method -Path $Path -Body $Body
    if ([string]::IsNullOrWhiteSpace($response.Content)) { return $null }
    return $response.Content | ConvertFrom-Json
}

function Assert-True([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw "MODEL ACCEPTANCE FAILED: $Message" }
}

function Resolve-Model([object[]]$Models, [string]$Requested, [int]$Index) {
    if (-not [string]::IsNullOrWhiteSpace($Requested)) {
        $matched = @($Models | Where-Object {
            [string]$_.id -eq $Requested -or [string]$_.external_id -eq $Requested
        }) | Select-Object -First 1
        Assert-True ($null -ne $matched) "requested model '$Requested' was not discovered for account '$AccountId'"
        return $matched
    }
    Assert-True ($Models.Count -gt $Index) "account '$AccountId' exposes fewer than $($Index + 1) discovered Gemini models"
    return $Models[$Index]
}

function New-Thread([string]$Title, [string]$ModelId) {
    $thread = Invoke-GatewayJson -Method POST -Path "/v1/threads" -Body @{
        title = $Title
        model = $ModelId
    }
    Assert-True (-not [string]::IsNullOrWhiteSpace([string]$thread.id)) "thread creation returned no id"
    $CreatedThreads.Add([string]$thread.id)
    return [string]$thread.id
}

function Send-Message([string]$ThreadId, [object]$Model, [string]$Prompt) {
    $response = Invoke-Gateway -Method POST -Path "/v1/threads/$ThreadId/messages" -Body @{
        content = $Prompt
        model = [string]$Model.id
        stream = $false
    }
    $expectedRoute = "discovered:${AccountId}:$($Model.external_id)"
    $actualRoute = [string]$response.Headers["x-llmgateway-route"]
    Assert-True ($actualRoute -eq $expectedRoute) "model '$($Model.id)' routed via '$actualRoute', expected '$expectedRoute'"
    $body = $response.Content | ConvertFrom-Json
    Assert-True (-not [string]::IsNullOrWhiteSpace([string]$body.choices[0].message.content)) "model '$($Model.id)' returned empty assistant content"

    Start-Sleep -Milliseconds 250
    $runtime = Invoke-GatewayJson -Method GET -Path "/_llmgateway/browser-accounts/$AccountId/runtime"
    Assert-True (-not [bool]$runtime.browser_running) "Chromium is running after model '$($Model.id)' turn"
    Assert-True ([string]$runtime.last_execution.transport -eq "direct-http") "model '$($Model.id)' did not use direct-http"
    Assert-True (-not [bool]$runtime.last_execution.browser_fallback) "model '$($Model.id)' used browser fallback"
    Assert-True ([string]$runtime.last_execution.adapter_id -eq "gemini-web-http") "model '$($Model.id)' used adapter '$($runtime.last_execution.adapter_id)'"
    Assert-True ([string]$runtime.last_execution.model -eq [string]$Model.external_id) "model '$($Model.id)' executed as '$($runtime.last_execution.model)', expected '$($Model.external_id)'"
}

function Send-StreamingMessage([string]$ThreadId, [object]$Model, [string]$Prompt) {
    $response = Invoke-Gateway -Method POST -Path "/v1/threads/$ThreadId/messages" -Body @{
        content = $Prompt
        model = [string]$Model.id
        stream = $true
    }
    $expectedRoute = "discovered:${AccountId}:$($Model.external_id)"
    $actualRoute = [string]$response.Headers["x-llmgateway-route"]
    Assert-True ($actualRoute -eq $expectedRoute) "stream model '$($Model.id)' routed via '$actualRoute', expected '$expectedRoute'"

    $sse = [string]$response.Content
    Assert-True ($sse -match '(?m)^data:\s*\[DONE\]\s*

function Get-Affinity([string]$ThreadId) {
    return Invoke-GatewayJson -Method GET -Path "/_llmgateway/threads/$ThreadId/browser-affinity/$AccountId"
}

function Cleanup {
    if ($KeepThreads) {
        Write-Host "[gemini-model-live] Keeping threads: $($CreatedThreads -join ', ')"
        return
    }
    foreach ($threadId in $CreatedThreads) {
        try {
            $null = Invoke-GatewayJson -Method DELETE -Path "/v1/threads/$threadId"
        } catch {
            Write-Warning "Failed to delete '$threadId': $($_.Exception.Message)"
        }
    }
}

try {
    Write-Host "[gemini-model-live] Refreshing Gemini model catalog for '$AccountId'"
    $refresh = Invoke-GatewayJson -Method POST -Path "/_llmgateway/accounts/$AccountId/models/refresh"
    Assert-True ([int]$refresh.discovered_models -gt 0) "model discovery returned zero models"

    $payload = Invoke-GatewayJson -Method GET -Path "/_llmgateway/accounts/$AccountId/models"
    $models = @($payload.data | Where-Object {
        $_.provider -eq "gemini-web" -and
        @($_.accounts | Where-Object {
            $_.account_id -eq $AccountId -and $_.enabled -and $_.availability -eq "available" -and $_.discovered
        }).Count -gt 0
    })
    Assert-True ($models.Count -ge 2) "need at least two discovered Gemini models to prove per-model selection"

    $selectedA = Resolve-Model $models $ModelA 0
    $selectedB = Resolve-Model $models $ModelB 1
    Assert-True ([string]$selectedA.id -ne [string]$selectedB.id) "ModelA and ModelB resolved to the same model"

    $publicModels = Invoke-GatewayJson -Method GET -Path "/v1/models"
    $publicIds = @($publicModels.data | ForEach-Object { [string]$_.id })
    Assert-True ($publicIds -contains [string]$selectedA.id) "/v1/models does not expose Model A '$($selectedA.id)'"
    Assert-True ($publicIds -contains [string]$selectedB.id) "/v1/models does not expose Model B '$($selectedB.id)'"

    Write-Host "[gemini-model-live] Model A: $($selectedA.display_name) [$($selectedA.id)]"
    Write-Host "[gemini-model-live] Model B: $($selectedB.display_name) [$($selectedB.id)]"

    $runtime = Invoke-GatewayJson -Method GET -Path "/_llmgateway/browser-accounts/$AccountId/runtime"
    Assert-True ([int]$runtime.model_catalog.count -ge 2) "runtime model catalog exposes fewer than two models"
    Assert-True (-not [bool]$runtime.model_catalog.refresh_required) "runtime model catalog still requires refresh"
    Assert-True (-not [string]::IsNullOrWhiteSpace([string]$runtime.model_catalog.discovered_at)) "runtime model catalog has no discovered_at timestamp"
    if ([bool]$runtime.browser_running) {
        $null = Invoke-GatewayJson -Method POST -Path "/_llmgateway/browser-sessions/$($runtime.session_id)/driver/stop"
        Start-Sleep -Milliseconds 300
    }

    $threadA = New-Thread "Gemini model A acceptance" ([string]$selectedA.id)
    Send-Message $threadA $selectedA "Reply with exactly: model-a"
    $affinityA = Get-Affinity $threadA
    Assert-True ($null -ne $affinityA.mapping) "Model A thread has no native Gemini conversation"

    $threadB = New-Thread "Gemini model B acceptance" ([string]$selectedB.id)
    Send-Message $threadB $selectedB "Reply with exactly: model-b"
    $affinityB = Get-Affinity $threadB
    Assert-True ($null -ne $affinityB.mapping) "Model B thread has no native Gemini conversation"

    $urlA = [string]$affinityA.mapping.conversation_url
    $urlB = [string]$affinityB.mapping.conversation_url
    Assert-True (-not [string]::IsNullOrWhiteSpace($urlA)) "Model A native URL is empty"
    Assert-True (-not [string]::IsNullOrWhiteSpace($urlB)) "Model B native URL is empty"
    Assert-True ($urlA -ne $urlB) "two local threads reused the same native Gemini conversation"

    $ordA = [int]$affinityA.mapping.last_synced_ordinal
    Send-Message $threadA $selectedA "Reply with exactly: model-a-continued"
    $affinityA2 = Get-Affinity $threadA
    Assert-True ([string]$affinityA2.mapping.conversation_url -eq $urlA) "Model A continuation changed native Gemini conversation"
    Assert-True ([int]$affinityA2.mapping.last_synced_ordinal -gt $ordA) "Model A continuation did not advance native affinity"

    $ordB = [int]$affinityB.mapping.last_synced_ordinal
    Send-Message $threadB $selectedB "Reply with exactly: model-b-continued"
    $affinityB2 = Get-Affinity $threadB
    Assert-True ([string]$affinityB2.mapping.conversation_url -eq $urlB) "Model B continuation changed native Gemini conversation"
    Assert-True ([int]$affinityB2.mapping.last_synced_ordinal -gt $ordB) "Model B continuation did not advance native affinity"

    $streamOrdA = [int]$affinityA2.mapping.last_synced_ordinal
    Send-StreamingMessage $threadA $selectedA "Reply with exactly: model-a-stream"
    $affinityA3 = Get-Affinity $threadA
    Assert-True ([string]$affinityA3.mapping.conversation_url -eq $urlA) "Model A streaming changed native Gemini conversation"
    Assert-True ([int]$affinityA3.mapping.last_synced_ordinal -gt $streamOrdA) "Model A streaming did not advance native affinity"

    $streamOrdB = [int]$affinityB2.mapping.last_synced_ordinal
    Send-StreamingMessage $threadB $selectedB "Reply with exactly: model-b-stream"
    $affinityB3 = Get-Affinity $threadB
    Assert-True ([string]$affinityB3.mapping.conversation_url -eq $urlB) "Model B streaming changed native Gemini conversation"
    Assert-True ([int]$affinityB3.mapping.last_synced_ordinal -gt $streamOrdB) "Model B streaming did not advance native affinity"

    Write-Host ""
    Write-Host "GEMINI BROWSERLESS MODEL SELECTION: PASS" -ForegroundColor Green
    Write-Host "Account: $AccountId"
    Write-Host "Model A: $($selectedA.id) -> discovered:${AccountId}:$($selectedA.external_id)"
    Write-Host "Model B: $($selectedB.id) -> discovered:${AccountId}:$($selectedB.external_id)"
    Write-Host "Chromium running: false"
} finally {
    Cleanup
}
) "stream model '$($Model.id)' ended without [DONE]"
    Assert-True ($sse -match '"content"') "stream model '$($Model.id)' returned no assistant delta"
    Assert-True ($sse -match '"finish_reason"\s*:\s*"[^"]+"') "stream model '$($Model.id)' returned no terminal finish_reason"
    $frames = [regex]::Matches($sse, '(?m)^data:\s*\{')
    Assert-True ($frames.Count -ge 2) "stream model '$($Model.id)' did not expose incremental SSE frames"

    Start-Sleep -Milliseconds 250
    $runtime = Invoke-GatewayJson -Method GET -Path "/_llmgateway/browser-accounts/$AccountId/runtime"
    Assert-True (-not [bool]$runtime.browser_running) "Chromium is running after streaming model '$($Model.id)'"
    Assert-True ([string]$runtime.last_execution.transport -eq "direct-http") "stream model '$($Model.id)' did not use direct-http"
    Assert-True (-not [bool]$runtime.last_execution.browser_fallback) "stream model '$($Model.id)' used browser fallback"
    Assert-True ([string]$runtime.last_execution.adapter_id -eq "gemini-web-http") "stream model '$($Model.id)' used adapter '$($runtime.last_execution.adapter_id)'"
    Assert-True ([string]$runtime.last_execution.model -eq [string]$Model.external_id) "stream model '$($Model.id)' executed as '$($runtime.last_execution.model)', expected '$($Model.external_id)'"

    $thread = Invoke-GatewayJson -Method GET -Path "/v1/threads/$ThreadId"
    $emptyAssistants = @($thread.messages | Where-Object {
        $_.role -eq "assistant" -and [string]::IsNullOrWhiteSpace([string]$_.content)
    })
    Assert-True ($emptyAssistants.Count -eq 0) "stream model '$($Model.id)' persisted an empty assistant message"
}

function Get-Affinity([string]$ThreadId) {
    return Invoke-GatewayJson -Method GET -Path "/_llmgateway/threads/$ThreadId/browser-affinity/$AccountId"
}

function Cleanup {
    if ($KeepThreads) {
        Write-Host "[gemini-model-live] Keeping threads: $($CreatedThreads -join ', ')"
        return
    }
    foreach ($threadId in $CreatedThreads) {
        try {
            $null = Invoke-GatewayJson -Method DELETE -Path "/v1/threads/$threadId"
        } catch {
            Write-Warning "Failed to delete '$threadId': $($_.Exception.Message)"
        }
    }
}

try {
    Write-Host "[gemini-model-live] Refreshing Gemini model catalog for '$AccountId'"
    $refresh = Invoke-GatewayJson -Method POST -Path "/_llmgateway/accounts/$AccountId/models/refresh"
    Assert-True ([int]$refresh.discovered_models -gt 0) "model discovery returned zero models"

    $payload = Invoke-GatewayJson -Method GET -Path "/_llmgateway/accounts/$AccountId/models"
    $models = @($payload.data | Where-Object {
        $_.provider -eq "gemini-web" -and
        @($_.accounts | Where-Object {
            $_.account_id -eq $AccountId -and $_.enabled -and $_.availability -eq "available" -and $_.discovered
        }).Count -gt 0
    })
    Assert-True ($models.Count -ge 2) "need at least two discovered Gemini models to prove per-model selection"

    $selectedA = Resolve-Model $models $ModelA 0
    $selectedB = Resolve-Model $models $ModelB 1
    Assert-True ([string]$selectedA.id -ne [string]$selectedB.id) "ModelA and ModelB resolved to the same model"

    Write-Host "[gemini-model-live] Model A: $($selectedA.display_name) [$($selectedA.id)]"
    Write-Host "[gemini-model-live] Model B: $($selectedB.display_name) [$($selectedB.id)]"

    $runtime = Invoke-GatewayJson -Method GET -Path "/_llmgateway/browser-accounts/$AccountId/runtime"
    if ([bool]$runtime.browser_running) {
        $null = Invoke-GatewayJson -Method POST -Path "/_llmgateway/browser-sessions/$($runtime.session_id)/driver/stop"
        Start-Sleep -Milliseconds 300
    }

    $threadA = New-Thread "Gemini model A acceptance" ([string]$selectedA.id)
    Send-Message $threadA $selectedA "Reply with exactly: model-a"
    $affinityA = Get-Affinity $threadA
    Assert-True ($null -ne $affinityA.mapping) "Model A thread has no native Gemini conversation"

    $threadB = New-Thread "Gemini model B acceptance" ([string]$selectedB.id)
    Send-Message $threadB $selectedB "Reply with exactly: model-b"
    $affinityB = Get-Affinity $threadB
    Assert-True ($null -ne $affinityB.mapping) "Model B thread has no native Gemini conversation"

    $urlA = [string]$affinityA.mapping.conversation_url
    $urlB = [string]$affinityB.mapping.conversation_url
    Assert-True (-not [string]::IsNullOrWhiteSpace($urlA)) "Model A native URL is empty"
    Assert-True (-not [string]::IsNullOrWhiteSpace($urlB)) "Model B native URL is empty"
    Assert-True ($urlA -ne $urlB) "two local threads reused the same native Gemini conversation"

    Write-Host ""
    Write-Host "GEMINI BROWSERLESS MODEL SELECTION: PASS" -ForegroundColor Green
    Write-Host "Account: $AccountId"
    Write-Host "Model A: $($selectedA.id) -> discovered:${AccountId}:$($selectedA.external_id)"
    Write-Host "Model B: $($selectedB.id) -> discovered:${AccountId}:$($selectedB.external_id)"
    Write-Host "Chromium running: false"
} finally {
    Cleanup
}
