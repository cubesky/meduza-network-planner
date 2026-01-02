# s6-overlay v3 Configuration Verification Script
# Run this to verify all s6 configuration is correct

Write-Host "`n=== s6-overlay v3 Configuration Verification ===" -ForegroundColor Cyan
Write-Host ""

$errors = 0
$warnings = 0

# 1. Check bundle structure
Write-Host "[1. Bundle Structure]" -ForegroundColor Yellow
if (Test-Path "s6-services\default\type") {
    $type = Get-Content "s6-services\default\type"
    if ($type -eq "bundle") {
        Write-Host "  ✅ default bundle: type=$type" -ForegroundColor Green
    } else {
        Write-Host "  ❌ ERROR: default/type should be 'bundle', not '$type'" -ForegroundColor Red
        $errors++
    }
} else {
    Write-Host "  ❌ default bundle missing type file" -ForegroundColor Red
    $errors++
}

if (Test-Path "s6-services\user\type") {
    $type = Get-Content "s6-services\user\type"
    if ($type -eq "bundle") {
        Write-Host "  ✅ user bundle: type=$type" -ForegroundColor Green
    } else {
        Write-Host "  ❌ ERROR: user/type should be 'bundle', not '$type'" -ForegroundColor Red
        $errors++
    }
} else {
    Write-Host "  ❌ user bundle missing type file" -ForegroundColor Red
    $errors++
}
Write-Host ""

# 2. Check bundle contents
Write-Host "[2. Bundle Contents]" -ForegroundColor Yellow
if (Test-Path "s6-services\default\contents.d") {
    $contents = (Get-ChildItem "s6-services\default\contents.d").Name -join ", "
    Write-Host "  ✅ default/contents.d exists" -ForegroundColor Green
    Write-Host "  Contents: $contents"
} else {
    Write-Host "  ❌ default/contents.d missing" -ForegroundColor Red
    $errors++
}

if (Test-Path "s6-services\user\contents.d") {
    $contents = (Get-ChildItem "s6-services\user\contents.d").Name -join ", "
    Write-Host "  ✅ user/contents.d exists" -ForegroundColor Green
    Write-Host "  Contents: $contents"
} else {
    Write-Host "  ❌ user/contents.d missing" -ForegroundColor Red
    $errors++
}
Write-Host ""

# 3. Check service types
Write-Host "[3. Service Types]" -ForegroundColor Yellow
$services = @('dbus', 'avahi', 'watchfrr', 'watcher', 'mihomo', 'easytier', 'tinc', 'mosdns', 'dnsmasq', 'dns-monitor')
foreach ($svc in $services) {
    if (Test-Path "s6-services\$svc\type") {
        $type = Get-Content "s6-services\$svc\type"
        Write-Host "  ✅ $svc`: type=$type" -ForegroundColor Green
    } else {
        Write-Host "  ❌ $svc`: missing type file" -ForegroundColor Red
        $errors++
    }
}
Write-Host ""

# 4. Check dependencies
Write-Host "[4. Service Dependencies]" -ForegroundColor Yellow
$depServices = @('dbus', 'avahi', 'watcher', 'watchfrr')
foreach ($svc in $depServices) {
    if (Test-Path "s6-services\$svc\dependencies.d") {
        $deps = (Get-ChildItem "s6-services\$svc\dependencies.d").Name -join ", "
        Write-Host "  ✅ $svc depends on: $deps" -ForegroundColor Green
        
        # Verify files are empty
        foreach ($depFile in Get-ChildItem "s6-services\$svc\dependencies.d") {
            if ($depFile.Length -gt 0) {
                Write-Host "  ❌ ERROR: $($depFile.Name) should be empty!" -ForegroundColor Red
                $errors++
            }
        }
    }
}
Write-Host ""

# 5. Check pipelines
Write-Host "[5. Pipeline Configurations]" -ForegroundColor Yellow
$pipelineServices = @('mihomo', 'watcher', 'tinc', 'mosdns', 'easytier', 'dnsmasq', 'dns-monitor')
foreach ($svc in $pipelineServices) {
    if (Test-Path "s6-services\$svc\producer-for") {
        $producer = Get-Content "s6-services\$svc\producer-for"
        $consumer = if (Test-Path "s6-services\$svc\log\consumer-for") { Get-Content "s6-services\$svc\log\consumer-for" } else { "missing" }
        $pipeline = if (Test-Path "s6-services\$svc\log\pipeline-name") { Get-Content "s6-services\$svc\log\pipeline-name" } else { "missing" }
        
        if ($consumer -ne "missing" -and $pipeline -ne "missing") {
            Write-Host "  ✅ $svc → $producer (pipeline: $pipeline)" -ForegroundColor Green
        } else {
            Write-Host "  ⚠️  $svc pipeline incomplete (consumer: $consumer, pipeline: $pipeline)" -ForegroundColor Yellow
            $warnings++
        }
    }
}
Write-Host ""

# 6. Check log scripts
Write-Host "[6. Log Script Syntax]" -ForegroundColor Yellow
$badLogs = 0
foreach ($logRun in Get-ChildItem -Path "s6-services\*\log\run" -Recurse -ErrorAction SilentlyContinue) {
    $content = Get-Content $logRun.FullName -Raw
    if ($content -match "s6-svlogd") {
        Write-Host "  ❌ $($logRun.Directory.Parent.Name) uses incorrect s6-svlogd syntax" -ForegroundColor Red
        $badLogs++
    } elseif ($content -match "logutil-service") {
        Write-Host "  ✅ $($logRun.Directory.Parent.Name) uses logutil-service (correct)" -ForegroundColor Green
    }
}
if ($badLogs -gt 0) {
    Write-Host "  ❌ Found $badLogs services with incorrect log syntax" -ForegroundColor Red
    $errors += $badLogs
}
Write-Host ""

# Summary
Write-Host "=== Verification Complete ===" -ForegroundColor Cyan
Write-Host ""
if ($errors -eq 0 -and $warnings -eq 0) {
    Write-Host "✅ Configuration is correct for s6-overlay v3!" -ForegroundColor Green
    Write-Host ""
    Write-Host "You can now rebuild the container:" -ForegroundColor White
    Write-Host "  docker compose down" -ForegroundColor Gray
    Write-Host "  docker compose build --no-cache" -ForegroundColor Gray
    Write-Host "  docker compose up -d" -ForegroundColor Gray
} else {
    if ($errors -gt 0) {
        Write-Host "❌ Found $errors error(s)" -ForegroundColor Red
    }
    if ($warnings -gt 0) {
        Write-Host "⚠️  Found $warnings warning(s)" -ForegroundColor Yellow
    }
    exit 1
}
