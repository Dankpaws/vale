[CmdletBinding()]
param(
    [string]$ProjectPath = "",
    [ValidateRange(1, 65535)]
    [int]$Port = 8080,
    [switch]$SkipHealthCheck
)

$ErrorActionPreference = "Stop"

function Stop-ValeInstall([string]$Message) {
    throw "Vale install error: $Message"
}

function Set-ValePort([string]$Path, [int]$Value) {
    $content = [System.IO.File]::ReadAllText($Path)
    $line = "VALE_PORT=$Value"
    $pattern = [regex]'(?m)^VALE_PORT=.*$'
    if ($pattern.IsMatch($content)) {
        $content = $pattern.Replace($content, $line, 1)
    } else {
        if ($content.Length -gt 0 -and -not $content.EndsWith("`n")) {
            $content += [Environment]::NewLine
        }
        $content += $line + [Environment]::NewLine
    }
    [System.IO.File]::WriteAllText(
        $Path,
        $content,
        [System.Text.UTF8Encoding]::new($false)
    )
}

if ([string]::IsNullOrWhiteSpace($ProjectPath)) {
    $ProjectPath = $PSScriptRoot
}

try {
    $resolvedProject = (Resolve-Path -LiteralPath $ProjectPath -ErrorAction Stop).Path
} catch {
    Stop-ValeInstall "project path is not accessible: $ProjectPath"
}

$composeFile = Join-Path $resolvedProject "compose.yaml"
$envExample = Join-Path $resolvedProject ".env.example"
$envFile = Join-Path $resolvedProject ".env"

if (-not (Test-Path -LiteralPath $composeFile -PathType Leaf)) {
    Stop-ValeInstall "compose.yaml was not found in $resolvedProject"
}
if (-not (Test-Path -LiteralPath $envExample -PathType Leaf)) {
    Stop-ValeInstall ".env.example was not found in $resolvedProject"
}

if ($null -eq (Get-Command docker -ErrorAction SilentlyContinue)) {
    Stop-ValeInstall "Docker Desktop is required; install it with the Linux container engine enabled"
}

$dockerOs = ((& docker info --format "{{.OSType}}" 2>$null) | Out-String).Trim()
if ($LASTEXITCODE -ne 0) {
    Stop-ValeInstall "the Docker daemon is not running; start Docker Desktop and retry"
}
if ($dockerOs -ne "linux") {
    Stop-ValeInstall "switch Docker Desktop to Linux containers; native Windows containers are not supported"
}

$composeVersion = ((& docker compose version --short 2>$null) | Out-String).Trim()
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($composeVersion)) {
    Stop-ValeInstall "the Docker Compose v2 plugin is required"
}

$portWasSpecified = $PSBoundParameters.ContainsKey("Port")
$createdEnv = $false
if (-not (Test-Path -LiteralPath $envFile -PathType Leaf)) {
    Copy-Item -LiteralPath $envExample -Destination $envFile
    $createdEnv = $true
    Write-Output "Created $envFile with nonsecret local defaults."
} else {
    Write-Output "Preserving existing $envFile."
}

$effectivePort = $Port
if (-not $createdEnv -and -not $portWasSpecified) {
    $envContent = [System.IO.File]::ReadAllText($envFile)
    $portMatch = [regex]::Match($envContent, '(?m)^VALE_PORT=([0-9]+)[ \t]*\r?$')
    if ($portMatch.Success) {
        $parsedPort = 0
        if (-not [int]::TryParse($portMatch.Groups[1].Value, [ref]$parsedPort) -or
            $parsedPort -lt 1 -or $parsedPort -gt 65535) {
            Stop-ValeInstall "VALE_PORT in .env must be a number from 1 to 65535"
        }
        $effectivePort = $parsedPort
    }
}
if ($createdEnv -or $portWasSpecified) {
    Set-ValePort -Path $envFile -Value $effectivePort
}

# Compose interpolation uses process environment values before .env values.
# Override only the requested host port; preserve account/cookie choices from
# an existing .env during upgrades.
$previousEnvironment = @{}
foreach ($name in @("VALE_PORT")) {
    $previousEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, "Process")
}
$env:VALE_PORT = "$effectivePort"

$composeArgs = @(
    "compose",
    "--project-name", "vale",
    "--project-directory", $resolvedProject,
    "--file", $composeFile
)

Push-Location -LiteralPath $resolvedProject
try {
    & docker @composeArgs config --quiet
    if ($LASTEXITCODE -ne 0) {
        Stop-ValeInstall "Compose configuration validation failed"
    }

    Write-Output "Building and starting Vale with Docker Desktop Linux containers..."
    & docker @composeArgs up --build --detach
    if ($LASTEXITCODE -ne 0) {
        Stop-ValeInstall "Docker Compose could not build or start Vale"
    }

    if (-not $SkipHealthCheck) {
        $healthy = $false
        for ($attempt = 0; $attempt -lt 60; $attempt++) {
            $null = & docker @composeArgs exec -T vale curl --fail --silent "http://127.0.0.1:8080/healthz" 2>$null
            if ($LASTEXITCODE -eq 0) {
                $healthy = $true
                break
            }
            Start-Sleep -Seconds 1
        }
        if (-not $healthy) {
            & docker @composeArgs ps
            & docker @composeArgs stop vale
            Stop-ValeInstall "Vale did not become healthy and was stopped; inspect the Compose service status"
        }
        Write-Output "Vale container is healthy."
    }
} finally {
    Pop-Location
    foreach ($name in $previousEnvironment.Keys) {
        $previous = $previousEnvironment[$name]
        if ($null -eq $previous) {
            Remove-Item -LiteralPath "Env:$name" -ErrorAction SilentlyContinue
        } else {
            Set-Item -LiteralPath "Env:$name" -Value $previous
        }
    }
}

Write-Output ""
Write-Output "Vale is ready at http://127.0.0.1:$effectivePort/"
Write-Output "Open /setup to create the first account; this installer never handles a password."
Write-Output "Profile and archive data persist in the Compose volume; cache data uses a separate volume."
Write-Output "Windows support uses Docker Desktop Linux containers.  A native Windows service/binary is not part of this release contract."
