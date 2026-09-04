$ErrorActionPreference = "Stop"

$env:LLMGATEWAY_API_KEY = "ci-local-key"
$env:FAKE_API_KEY = "fake-key"

$work = Join-Path $env:RUNNER_TEMP "llmgateway-chromium-windows-smoke"
$profileRoot = Join-Path $work "profiles"
$configPath = Join-Path $work "llmgateway.toml"
$stdoutPath = Join-Path $work "llmgateway.stdout.log"
$stderrPath = Join-Path $work "llmgateway.stderr.log"
$dbPath = Join-Path (Get-Location) "data\llmgateway-windows-chromium-smoke.db"
$gateway = $null

function To-TomlPath([string] $Path) {
    return $Path.Replace("\", "/")
}

function Show-GatewayLogs {
    if (Test-Path $stdoutPath) {
        Write-Host "--- llmgateway stdout ---"
        Get-Content $stdoutPath -ErrorAction SilentlyContinue
    }
    if (Test-Path $stderrPath) {
        Write-Host "--- llmgateway stderr ---"
        Get-Content $stderrPath -ErrorAction SilentlyContinue
    }
}

$chromeCandidates = @()
if ($env:ProgramFiles) {
    $chromeCandidates += (Join-Path $env:ProgramFiles "Google\Chrome\Application\chrome.exe")
}
$programFilesX86 = [Environment]::GetEnvironmentVariable("ProgramFiles(x86)")
if ($programFilesX86) {
    $chromeCandidates += (Join-Path $programFilesX86 "Google\Chrome\Application\chrome.exe")
}
if ($env:LOCALAPPDATA) {
    $chromeCandidates += (Join-Path $env:LOCALAPPDATA "Google\Chrome\Application\chrome.exe")
}
$chromeCommand = Get-Command chrome.exe -ErrorAction SilentlyContinue
if ($chromeCommand) {
    $chromeCandidates += $chromeCommand.Source
}
$chrome = $chromeCandidates | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
if (-not $chrome) {
    throw "Google Chrome was not found on the Windows CI runner"
}

Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
Remove-Item -Force $dbPath, "$dbPath-shm", "$dbPath-wal" -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $work, $profileRoot, (Split-Path $dbPath -Parent) | Out-Null

$chromeToml = To-TomlPath $chrome
$profileToml = To-TomlPath $profileRoot

@"
[server]
host = "127.0.0.1"
port = 7332

[api]
key_env = "LLMGATEWAY_API_KEY"
default_model = "llmgateway-auto"

[storage]
database_url = "sqlite://data/llmgateway-windows-chromium-smoke.db"

[browser]
enabled = true
profile_root = "$profileToml"

[browser.sessions.windows-chrome]
provider = "fake"
label = "Windows Chrome smoke"
login_url = "https://example.com"
enabled = true

[chromium]
enabled = true
executable = "$chromeToml"
startup_timeout_seconds = 20
auto_recover = false
reconcile_interval_seconds = 15
extra_args = ["--headless=new", "--disable-gpu", "--disable-background-networking"]

[chromium.sessions.windows-chrome]
enabled = true
ready_url_prefixes = []

[context]
enabled = false
retrieval_enabled = false

[[providers]]
id = "fake"
kind = "openai-compatible"
base_url = "http://127.0.0.1:18080/v1"
models_path = "models"

[[accounts]]
id = "fake-api"
provider = "fake"
api_key_env = "FAKE_API_KEY"
auth_style = "bearer"
enabled = true
discover_models = false

[[routes]]
id = "fake-route"
account = "fake-api"
model = "fake-model"
priority = 10
enabled = true
capabilities = ["chat"]

[virtual_models.llmgateway-auto]
routes = ["fake-route"]
[virtual_models.llmgateway-coding]
routes = ["fake-route"]
[virtual_models.llmgateway-best]
routes = ["fake-route"]
"@ | Set-Content -Encoding UTF8 $configPath

$env:LLMGATEWAY_CONFIG = $configPath

try {
    cargo build --quiet
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed with exit code $LASTEXITCODE"
    }

    $gateway = Start-Process -PassThru -FilePath ".\target\debug\llmgateway.exe" -RedirectStandardOutput $stdoutPath -RedirectStandardError $stderrPath

    $healthy = $false
    for ($i = 0; $i -lt 80; $i++) {
        & curl.exe -fsS "http://127.0.0.1:7332/_llmgateway/health" *> $null
        if ($LASTEXITCODE -eq 0) {
            $healthy = $true
            break
        }
        Start-Sleep -Milliseconds 250
    }
    if (-not $healthy) {
        Show-GatewayLogs
        throw "llmgateway did not become healthy"
    }

    $headers = @{ Authorization = "Bearer $($env:LLMGATEWAY_API_KEY)" }
    $launch = Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:7332/_llmgateway/browser-sessions/windows-chrome/driver/launch" -Headers $headers
    if (-not $launch.launched -or [int]$launch.launch.debugger_port -le 0) {
        throw "Chromium launch did not return a valid debugger port"
    }
    if (-not [IO.Path]::IsPathRooted([string]$launch.launch.profile_dir)) {
        throw "Chromium launch did not return an absolute isolated profile path"
    }

    $status = Invoke-RestMethod -Uri "http://127.0.0.1:7332/_llmgateway/browser-sessions/windows-chrome/driver/status" -Headers $headers
    if (-not $status.running -or -not $status.debugger_reachable) {
        throw "Chromium CDP is not reachable after launch"
    }
    if ([int]$status.debugger_port -ne [int]$launch.launch.debugger_port) {
        throw "Chromium status debugger port does not match launch"
    }

    $portFile = Join-Path $profileRoot "windows-chrome\DevToolsActivePort"
    if (-not (Test-Path $portFile)) {
        throw "llmgateway did not persist DevToolsActivePort"
    }
    $persistedPort = [int](Get-Content $portFile | Select-Object -First 1)
    if ($persistedPort -ne [int]$launch.launch.debugger_port) {
        throw "persisted DevToolsActivePort does not match reachable CDP port"
    }

    $stop = Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:7332/_llmgateway/browser-sessions/windows-chrome/driver/stop" -Headers $headers
    if (-not $stop.stopped) {
        throw "Chromium stop endpoint did not confirm stop"
    }

    Write-Host "llmgateway real Chrome Windows CDP smoke test passed"
} catch {
    Show-GatewayLogs
    throw
} finally {
    if ($gateway -and -not $gateway.HasExited) {
        Stop-Process -Id $gateway.Id -Force -ErrorAction SilentlyContinue
        $gateway.WaitForExit()
    }
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
    Remove-Item -Force $dbPath, "$dbPath-shm", "$dbPath-wal" -ErrorAction SilentlyContinue
}
