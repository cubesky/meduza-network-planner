# Quick check for s6-services empty files git status (PowerShell)

Write-Host "`n=== s6-services Empty Files Status ===" -ForegroundColor Cyan
Write-Host ""

# Count total empty files
$totalEmpty = (Get-ChildItem -Path "s6-services" -Recurse -File | Where-Object { $_.Length -eq 0 }).Count
Write-Host "Total empty files in s6-services: $totalEmpty"

# Count tracked empty files
$trackedFiles = git ls-files s6-services
$trackedEmpty = (Get-ChildItem -Path "s6-services" -Recurse -File | Where-Object { 
    $_.Length -eq 0 -and 
    $trackedFiles -contains ($_.FullName.Replace((Get-Location).Path + "\", "").Replace("\", "/"))
}).Count

Write-Host "Tracked by git: $trackedEmpty"

if ($totalEmpty -eq $trackedEmpty) {
    Write-Host "✅ All empty files are tracked" -ForegroundColor Green
} else {
    Write-Host "⚠️  WARNING: $($totalEmpty - $trackedEmpty) empty files are NOT tracked!" -ForegroundColor Yellow
    Write-Host ""
    Write-Host "Untracked files:" -ForegroundColor Yellow
    
    Get-ChildItem -Path "s6-services" -Recurse -File | Where-Object { 
        $_.Length -eq 0 
    } | ForEach-Object {
        $relativePath = $_.FullName.Replace((Get-Location).Path + "\", "").Replace("\", "/")
        if ($trackedFiles -notcontains $relativePath) {
            Write-Host "  - $relativePath"
        }
    }
    
    Write-Host ""
    Write-Host "Run: git add s6-services/" -ForegroundColor Yellow
}

Write-Host ""

# Check for staged deletions
Write-Host "Checking for staged deletions of important files..." -ForegroundColor Cyan
$deleted = git diff --cached --name-only --diff-filter=D | Where-Object {
    $_ -match "^s6-services/" -and ($_ -match "dependencies\.d/" -or $_ -match "contents\.d/")
}

if ($deleted) {
    Write-Host "⚠️  WARNING: Important files are staged for deletion:" -ForegroundColor Yellow
    $deleted | ForEach-Object { Write-Host "  - $_" }
} else {
    Write-Host "✅ No important files staged for deletion" -ForegroundColor Green
}

Write-Host ""
