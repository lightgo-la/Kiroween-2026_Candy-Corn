#Requires -Version 5.1
<#
.SYNOPSIS
Regenerates the synthetic sample data files in samples/.

.DESCRIPTION
Every value is made up: names, IDs, amounts and comments are synthetic. A fixed seed makes every run write the same
files. Each file goes through a different input path of the profiler (delimiter, text encoding, JSON flavor) and has a
few planted anomalies to analyze; samples/README.md lists them.

samples/sales.csv, samples/flow.canvas.json, the .kiro blueprints and samples/replies/ are written by hand and are not
touched.

.EXAMPLE
powershell -ExecutionPolicy Bypass -File tools\gen-samples.ps1
#>
[CmdletBinding()]
param(
    [string]$OutDir = ''
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$inv = [Globalization.CultureInfo]::InvariantCulture
$rng = New-Object System.Random 20261119
if (-not $OutDir) { $OutDir = Join-Path (Split-Path -Parent $MyInvocation.MyCommand.Path) '..\samples' }
$OutDir = [IO.Path]::GetFullPath($OutDir)

function U([double]$a, [double]$b) { $a + ($b - $a) * $rng.NextDouble() }
function I([int]$a, [int]$b) { $rng.Next($a, $b + 1) }
function Pick([object[]]$items) { $items[$rng.Next($items.Count)] }
# Weighted pick from pairs of (value, weight): PickW @(@('a', 50), @('b', 30))
function PickW([object[]]$pairs) {
    $total = 0.0
    foreach ($p in $pairs) { $total += $p[1] }
    $r = $rng.NextDouble() * $total
    foreach ($p in $pairs) {
        $r -= $p[1]
        if ($r -lt 0) { return $p[0] }
    }
    $pairs[-1][0]
}
function F([double]$x, [string]$format) { $x.ToString($format, $inv) }
function D([datetime]$t, [string]$format) { $t.ToString($format, $inv) }
function Clamp([double]$x, [double]$lo, [double]$hi) { [math]::Min($hi, [math]::Max($lo, $x)) }

# Quotes a delimited-text field when it contains the delimiter, a quote or a line break.
function Q([string]$s, [string]$delim) {
    if ($s.Contains($delim) -or $s.Contains('"') -or $s.Contains("`n") -or $s.Contains("`r")) {
        return '"' + $s.Replace('"', '""') + '"'
    }
    $s
}
function Row([object[]]$cells, [string]$delim) {
    ($cells | ForEach-Object { Q ([string]$_) $delim }) -join $delim
}

function Save([string]$name, [string]$text, [Text.Encoding]$enc) {
    $path = Join-Path $OutDir $name
    [IO.File]::WriteAllText($path, $text, $enc)
    Write-Host ('{0,-26} {1,8:N1} KB' -f $name, ((Get-Item $path).Length / 1KB))
}

$utf8 = New-Object Text.UTF8Encoding $false
$utf8Bom = New-Object Text.UTF8Encoding $true
$cp949 = [Text.Encoding]::GetEncoding(949)
$utf16 = [Text.Encoding]::Unicode # UTF-16 LE; WriteAllText adds the BOM

# ---------- web_traffic.tsv: tab-separated, UTF-8, LF ----------
# Daily analytics for 4 pages over 60 days. Planted: a bot spike on /pricing on 2026-09-05 (sessions, users and bounce
# rate far above normal) and a tracking outage on 2026-09-12 (conversions and revenue empty).
$pages = @(
    @{ path = '/'; s = 3800, 4600; b = 0.38, 0.46; t = 95, 140; c = 40, 60; aov = 48, 62 },
    @{ path = '/pricing'; s = 1100, 1400; b = 0.30, 0.38; t = 120, 180; c = 25, 40; aov = 52, 70 },
    @{ path = '/blog'; s = 2200, 2900; b = 0.55, 0.65; t = 160, 260; c = 8, 15; aov = 40, 55 },
    @{ path = '/signup'; s = 600, 800; b = 0.20, 0.28; t = 60, 95; c = 90, 120; aov = 45, 58 }
)
$sb = New-Object Text.StringBuilder
[void]$sb.Append("date`tpage`tsessions`tusers`tbounce_rate`tavg_session_sec`tconversions`trevenue_usd`n")
$day0 = [datetime]'2026-08-01'
for ($d = 0; $d -lt 60; $d++) {
    $day = $day0.AddDays($d)
    $date = D $day 'yyyy-MM-dd'
    $weekend = ($day.DayOfWeek -eq [DayOfWeek]::Saturday) -or ($day.DayOfWeek -eq [DayOfWeek]::Sunday)
    $f = if ($weekend) { 0.75 } else { 1.0 }
    foreach ($pg in $pages) {
        $sessions = [int][math]::Round((U $pg.s[0] $pg.s[1]) * $f)
        $users = [int][math]::Round($sessions * (U 0.78 0.86))
        $bounce = U $pg.b[0] $pg.b[1]
        $sec = [int][math]::Round((U $pg.t[0] $pg.t[1]))
        $conv = [int][math]::Round((U $pg.c[0] $pg.c[1]) * $f)
        $rev = F ($conv * (U $pg.aov[0] $pg.aov[1])) '0.00'
        $convText = [string]$conv
        if ($date -eq '2026-09-05' -and $pg.path -eq '/pricing') {
            $sessions = 48210; $users = 47350; $bounce = 0.97; $sec = 3
        }
        if ($date -eq '2026-09-12') { $convText = ''; $rev = '' }
        $cells = $date, $pg.path, $sessions, $users, (F $bounce '0.000'), $sec, $convText, $rev
        [void]$sb.Append(($cells -join "`t") + "`n")
    }
}
Save 'web_traffic.tsv' $sb.ToString() $utf8

# ---------- iot_sensors.ndjson: one JSON object per line, UTF-8, LF ----------
# 6 sensors on 2 production lines, a reading every 30 minutes for 25 hours. Planted: D-04 overheats once (92.4 C,
# status fault), D-05's vibration climbs over its last 10 readings (bearing wear), D-06's battery fails at the end, and
# D-02 drops its humidity reading every 7th time (null).
$sb = New-Object Text.StringBuilder
$t0 = [datetime]'2026-10-03T08:00:00'
for ($k = 0; $k -lt 50; $k++) {
    $ts = D ($t0.AddMinutes(30 * $k)) "yyyy-MM-dd'T'HH:mm:ss'Z'"
    for ($i = 1; $i -le 6; $i++) {
        $dev = 'D-0' + $i
        $line = if ($i -le 3) { 'A' } else { 'B' }
        $temp = U 38 44
        $hum = F (U 35 55) '0.0'
        $vib = U 1.2 3.0
        $bat = 4.1 - 0.004 * $k - (U 0 0.05)
        $status = 'ok'
        if ($dev -eq 'D-05' -and $k -ge 40) { $vib = 3.0 + ($k - 39) * 0.48 }
        if ($dev -eq 'D-06' -and $k -ge 44) { $bat = 3.6 - ($k - 43) * 0.085 }
        if ($dev -eq 'D-02' -and ($k % 7) -eq 3) { $hum = 'null' }
        if ($dev -eq 'D-04' -and $k -eq 37) { $temp = 92.4; $status = 'fault' }
        elseif ($vib -gt 5 -or $bat -lt 3.4) { $status = 'warn' }
        [void]$sb.Append('{"device_id":"' + $dev + '","line":"' + $line + '","ts":"' + $ts + '","temperature_c":' +
            (F $temp '0.0') + ',"humidity_pct":' + $hum + ',"vibration_mm_s":' + (F $vib '0.00') + ',"battery_v":' +
            (F $bat '0.00') + ',"status":"' + $status + '"}' + "`n")
    }
}
Save 'iot_sensors.ndjson' $sb.ToString() $utf8

# ---------- ecommerce_orders.json: one pretty-printed JSON array, UTF-8, LF ----------
# 200 October orders. Planted: a bulk B2B order (quantity 480), a shipment that took 21 days, and 12 orders without a
# recorded discount (null).
$cats = @(
    @{ name = 'Electronics'; p = 89, 299 },
    @{ name = 'Home'; p = 25, 220 },
    @{ name = 'Sports'; p = 30, 260 },
    @{ name = 'Beauty'; p = 12, 90 },
    @{ name = 'Books'; p = 10, 60 }
)
$customers = 1..120 | ForEach-Object { 'C-' + (2000 + $_) }
$nullDiscount = @{}
while ($nullDiscount.Count -lt 12) { $nullDiscount[(I 0 199)] = $true }
$items = New-Object System.Collections.Generic.List[string]
$oct1 = [datetime]'2026-10-01'
for ($n = 0; $n -lt 200; $n++) {
    $cat = Pick $cats
    $catName = $cat.name
    $qty = PickW @(@(1, 50), @(2, 30), @(3, 15), @(4, 5))
    $price = F (U $cat.p[0] $cat.p[1]) '0.00'
    $disc = if ($nullDiscount.ContainsKey($n)) { 'null' } else {
        [string](PickW @(@(0, 55), @(5, 15), @(10, 15), @(15, 10), @(20, 5)))
    }
    $ship = PickW @(@(1, 15), @(2, 35), @(3, 30), @(4, 12), @(5, 8))
    $returned = if ($rng.NextDouble() -lt 0.08) { 'true' } else { 'false' }
    if ($n -eq 137) { $catName = 'Books'; $qty = 480; $price = '11.90'; $disc = '20' }
    if ($n -eq 61) { $ship = 21 }
    $date = D ($oct1.AddDays([math]::Floor($n * 31 / 200))) 'yyyy-MM-dd'
    $fields = @(
        ('"order_id": "SO-' + (104001 + $n) + '"'),
        ('"customer_id": "' + (Pick $customers) + '"'),
        ('"order_date": "' + $date + '"'),
        ('"channel": "' + (PickW @(@('web', 50), @('app', 35), @('marketplace', 15))) + '"'),
        ('"category": "' + $catName + '"'),
        ('"quantity": ' + $qty),
        ('"unit_price": ' + $price),
        ('"discount_pct": ' + $disc),
        ('"payment_method": "' + (PickW @(@('card', 60), @('paypal', 20), @('bank_transfer', 12), @('cod', 8))) + '"'),
        ('"shipping_days": ' + $ship),
        ('"returned": ' + $returned)
    )
    $items.Add("  {`n    " + ($fields -join ",`n    ") + "`n  }")
}
Save 'ecommerce_orders.json' ("[`n" + ($items -join ",`n") + "`n]`n") $utf8

# ---------- hr_employees_kr.csv: Korean headers and text, CP949 (Korean Excel "CSV" export), CRLF ----------
# 150 employees. Salaries are in 10,000 KRW with thousands separators, so they are quoted. Planted: one executive hired
# in 2024 at 25,000 (far above every other band), and an empty performance grade for this year's new hires.
$surnames = '김', '이', '박', '최', '정', '강', '조', '윤', '장', '임', '한', '오', '서', '신', '권', '황', '안', '송', '류', '홍'
$syll = '민', '서', '지', '현', '준', '우', '예', '도', '하', '윤', '수', '진', '영', '연', '은', '호', '성', '재', '원', '아', '유', '태', '혜', '승'
$depts = @(@('영업', 22), @('마케팅', 12), @('개발', 30), @('인사', 8), @('재무', 9), @('고객지원', 11), @('생산', 8))
$sites = @(@('서울', 50), @('판교', 20), @('부산', 12), @('대전', 10), @('광주', 8))
$bands = @{ '사원' = 3600, 4400; '대리' = 4300, 5300; '과장' = 5200, 6500; '차장' = 6300, 7800; '부장' = 7500, 9800 }
$asOf = [datetime]'2026-11-01'
$hireFrom = [datetime]'2008-01-02'
$span = ($asOf.AddDays(-20) - $hireFrom).TotalDays
$staff = New-Object System.Collections.Generic.List[object]
for ($n = 0; $n -lt 149; $n++) {
    $hire = $hireFrom.AddDays([math]::Floor($rng.NextDouble() * $span))
    $years = [int][math]::Floor(($asOf - $hire).TotalDays / 365.25)
    if ($years -lt 2) { $rank = '사원' }
    elseif ($years -lt 5) { $rank = Pick '사원', '대리', '대리' }
    elseif ($years -lt 9) { $rank = Pick '대리', '과장', '과장' }
    elseif ($years -lt 14) { $rank = Pick '과장', '차장', '차장' }
    else { $rank = Pick '차장', '부장', '부장' }
    $band = $bands[$rank]
    $salary = [math]::Round((U $band[0] $band[1]) / 10) * 10
    $grade = if ($hire.Year -eq 2026) { '' } else { PickW @(@('S', 8), @('A', 25), @('B', 50), @('C', 17)) }
    $staff.Add([pscustomobject]@{
            hire = $hire; years = $years; rank = $rank; salary = $salary; grade = $grade
            name = (Pick $surnames) + (Pick $syll) + (Pick $syll)
            dept = PickW $depts; site = PickW $sites
            remote = $(if ($rng.NextDouble() -lt 0.35) { 'Y' } else { 'N' })
        })
}
$exec = [datetime]'2024-03-04'
$staff.Add([pscustomobject]@{
        hire = $exec; years = [int][math]::Floor(($asOf - $exec).TotalDays / 365.25); rank = '상무'; salary = 25000
        grade = 'A'; name = '정태윤'; dept = '개발'; site = '서울'; remote = 'N'
    })
$lines = New-Object System.Collections.Generic.List[string]
$lines.Add('사원번호,이름,부서,직급,입사일,근속연수,연봉_만원,성과등급,재택근무,근무지')
$seq = @{}
foreach ($e in ($staff | Sort-Object { $_.hire })) {
    $y = $e.hire.Year
    $seq[$y] = 1 + $(if ($seq.ContainsKey($y)) { $seq[$y] } else { 0 })
    $id = 'K{0}{1:000}' -f $y, $seq[$y]
    $lines.Add((Row @($id, $e.name, $e.dept, $e.rank, (D $e.hire 'yyyy-MM-dd'), $e.years, (F $e.salary '#,##0'),
                $e.grade, $e.remote, $e.site) ','))
}
Save 'hr_employees_kr.csv' (($lines -join "`r`n") + "`r`n") $cp949

# ---------- support_tickets.csv: semicolon-separated, UTF-8 with BOM (Excel "CSV UTF-8", European list separator), CRLF
# 220 October tickets. Subjects contain commas, quotes and one semicolon, so some are quoted. Planted: an email answered
# after a weekend (2,880 minutes), a bug ticket open for 13 days (312 hours), and no CSAT for unrated tickets (empty).
$subjects = @{
    billing         = 'Charged twice; need a refund', 'Invoice shows the wrong VAT number', 'Card declined at checkout, but the bank says OK'
    login           = 'Password reset email never arrives', '2FA code rejected after phone change', 'Locked out after 3 attempts'
    shipping        = 'Package stuck in transit for 6 days', 'Delivered to the wrong address', 'Tracking link shows "label created" only'
    bug             = '"Pay" button does nothing on iOS 19', 'Export to CSV drops the last row', 'Dashboard freezes with large files'
    feature_request = 'Please add dark mode to the mobile app', 'Bulk edit for product tags', 'Webhook for refunds'
    refund          = 'Refund not received, order #10234', 'Partial refund for a damaged item', 'Refund to a closed card'
}
$catsT = @(@('billing', 20), @('login', 18), @('shipping', 20), @('bug', 17), @('feature_request', 10), @('refund', 15))
$agents = 'Mina', 'Jae', 'Sofia', 'Daniel', 'Priya', 'Tom'
$tickets = New-Object System.Collections.Generic.List[object]
for ($n = 0; $n -lt 220; $n++) {
    $created = $oct1.AddMinutes([math]::Floor((U 0 30) * 1440)).Date.AddMinutes((I 420 1380))
    $channel = PickW @(@('email', 35), @('chat', 30), @('phone', 15), @('web_form', 20))
    $priority = PickW @(@('low', 30), @('medium', 40), @('high', 22), @('urgent', 8))
    $cat = PickW $catsT
    switch ($channel) {
        'chat' { $first = I 1 8 }
        'phone' { $first = I 0 3 }
        'email' { $first = I 20 240 }
        default { $first = I 30 300 }
    }
    switch ($priority) {
        'urgent' { $res = U 0.5 8 }
        'high' { $res = U 2 24 }
        'medium' { $res = U 4 48 }
        default { $res = U 8 72 }
    }
    $tickets.Add([pscustomobject]@{
            created = $created; channel = $channel; priority = $priority; cat = $cat; first = $first; res = $res
            reopened = $(if ($rng.NextDouble() -lt 0.1) { 'yes' } else { 'no' })
            csat = $(if ($rng.NextDouble() -lt 0.25) { '' } else { [string](PickW @(@(5, 40), @(4, 30), @(3, 15), @(2, 8), @(1, 7))) })
            agent = Pick $agents; subject = Pick $subjects[$cat]
        })
}
$tickets[17].created = [datetime]'2026-10-09 18:40'; $tickets[17].channel = 'email'; $tickets[17].first = 2880
$tickets[90].cat = 'bug'; $tickets[90].priority = 'high'; $tickets[90].res = 312.0; $tickets[90].reopened = 'yes'
$tickets[90].csat = '1'; $tickets[90].subject = 'Export to CSV drops the last row'
$lines = New-Object System.Collections.Generic.List[string]
$lines.Add('ticket_id;created_at;channel;priority;category;first_response_min;resolution_hours;reopened;csat;agent;subject')
$n = 0
foreach ($t in ($tickets | Sort-Object { $_.created })) {
    $n++
    $lines.Add((Row @(('T-{0}' -f (50000 + $n)), (D $t.created 'yyyy-MM-dd HH:mm'), $t.channel, $t.priority, $t.cat, $t.first,
                (F $t.res '0.0'), $t.reopened, $t.csat, $t.agent, $t.subject) ';'))
}
Save 'support_tickets.csv' (($lines -join "`r`n") + "`r`n") $utf8Bom

# ---------- bank_transactions.txt: pipe-separated, UTF-8, LF ----------
# Two months of one checking account. Amounts and balances use thousands separators. Planted: three 1.00 card checks
# from an unknown merchant at night followed by a 9,850.00 wire to an unknown beneficiary (card testing, then fraud).
# Salary credits are also statistical outliers but normal for the account.
$tx = New-Object System.Collections.Generic.List[object]
function AddTx([datetime]$t, [string]$merchant, [string]$category, [double]$amount, [string]$channel) {
    $tx.Add([pscustomobject]@{ t = $t; merchant = $merchant; category = $category; amount = $amount; channel = $channel })
}
foreach ($m in 9, 10) {
    $first = [datetime]::new(2026, $m, 1)
    AddTx $first.AddHours(9) 'Hillside Apartments' 'rent' -1450 'ach'
    AddTx $first.AddDays(14).AddHours(6) 'ACME CORP PAYROLL' 'income' 4250 'ach'
    AddTx $first.AddDays(29).AddHours(6) 'ACME CORP PAYROLL' 'income' 4250 'ach'
    AddTx $first.AddDays(15).AddHours(10) 'TRANSFER TO SAVINGS SAV-2001' 'transfer' -500 'online'
    AddTx $first.AddDays(4).AddHours(11) 'City Power & Light' 'utilities' (-[math]::Round((U 62 108), 2)) 'ach'
    AddTx $first.AddDays(7).AddHours(11) 'FiberNet' 'utilities' -49.99 'ach'
    AddTx $first.AddDays(11).AddHours(11) 'Mobile One' 'utilities' -35 'ach'
    AddTx $first.AddDays(1).AddHours(5) 'Gym Pass' 'subscription' -39 'card'
    AddTx $first.AddDays(2).AddHours(5) 'StreamFlix' 'subscription' -15.49 'card'
    AddTx $first.AddDays(19).AddHours(5) 'CloudBox' 'subscription' -9.99 'card'
}
$daily = @(
    @{ p = 0.70; m = 'FreshMart', 'Green Grocer'; c = 'groceries'; r = 18, 140; ch = 'card' },
    @{ p = 0.85; m = 'Bean There', 'Daily Grind'; c = 'coffee'; r = 3.5, 7.2; ch = 'card' },
    @{ p = 0.70; m = 'Noodle House', 'Taco Loco', 'Sushi Go', 'Burger Barn'; c = 'restaurants'; r = 12, 85; ch = 'card' },
    @{ p = 0.60; m = 'Metro Transit', 'RideNow'; c = 'transport'; r = 2.5, 45; ch = 'card' },
    @{ p = 0.45; m = 'ShopZone', 'BookNook Online'; c = 'shopping'; r = 15, 220; ch = 'card_not_present' },
    @{ p = 0.12; m = 'QuickFuel', 'QuickFuel'; c = 'fuel'; r = 35, 70; ch = 'card' },
    @{ p = 0.08; m = 'CarePlus Pharmacy', 'CarePlus Pharmacy'; c = 'health'; r = 8, 60; ch = 'card' }
)
$sep1 = [datetime]'2026-09-01'
for ($d = 0; $d -lt 61; $d++) {
    $day = $sep1.AddDays($d)
    foreach ($kind in $daily) {
        if ($rng.NextDouble() -lt $kind.p) {
            AddTx $day.AddMinutes((I 420 1350)) (Pick $kind.m) $kind.c (-[math]::Round((U $kind.r[0] $kind.r[1]), 2)) $kind.ch
        }
    }
    if ($rng.NextDouble() -lt 0.06) { AddTx $day.AddMinutes((I 480 1260)) 'ATM WITHDRAWAL' 'cash' (-(Pick 40, 60, 100)) 'atm' }
}
foreach ($d in 6, 23, 41) {
    AddTx $sep1.AddDays($d).AddMinutes((I 600 1080)) 'ShopZone REFUND' 'refund' ([math]::Round((U 12 80), 2)) 'card_not_present'
}
foreach ($minute in 161, 163, 164) {
    $at = ([datetime]'2026-10-17').AddMinutes($minute)
    AddTx $at 'AUTH CHECK *ZX81' 'other' -1 'card_not_present'
}
$at = [datetime]'2026-10-17 03:12'
AddTx $at 'WIRE TRANSFER - UNKNOWN BENEFICIARY' 'transfer' -9850 'online'
$balance = 12480.15
$lines = New-Object System.Collections.Generic.List[string]
$lines.Add('txn_id|posted_at|account|merchant|category|amount|currency|balance_after|channel')
$n = 0
foreach ($t in ($tx | Sort-Object { $_.t })) {
    $n++
    $balance = [math]::Round($balance + $t.amount, 2)
    $lines.Add((@(('TX{0:0000000}' -f (4100000 + $n)), (D $t.t 'yyyy-MM-dd HH:mm'), 'CHK-1001', $t.merchant, $t.category,
                (F $t.amount '#,##0.00'), 'USD', (F $balance '#,##0.00'), $t.channel) -join '|'))
}
Save 'bank_transactions.txt' (($lines -join "`n") + "`n") $utf8

# ---------- energy_meter_utf16.txt: tab-separated, UTF-16 LE with BOM (Excel "Unicode Text"), CRLF ----------
# Daily readings of 4 buildings for 45 days. Planted: an HVAC fault at HQ Tower on 2026-09-22 (about 4 times the normal
# energy and peak load) while the weather and occupancy were normal.
$buildings = @(
    @{ code = 'EM-A01'; name = 'HQ Tower'; wd = 5200, 6400; we = 2000, 2600; occ = 70, 92, 5, 15 },
    @{ code = 'EM-B07'; name = 'R&D Center'; wd = 3100, 3900; we = 1500, 1900; occ = 60, 85, 10, 25 },
    @{ code = 'EM-C12'; name = 'Warehouse 2'; wd = 900, 1300; we = 500, 700; occ = 40, 60, 20, 35 },
    @{ code = 'EM-D03'; name = 'Data Center'; wd = 11800, 12600; we = 11800, 12600; occ = 2, 5, 2, 5 }
)
$lines = New-Object System.Collections.Generic.List[string]
$lines.Add("meter_code`tbuilding`tdate`tkwh`tpeak_kw`toutdoor_temp_c`toccupancy_pct`ttariff")
for ($d = 0; $d -lt 45; $d++) {
    $day = $sep1.AddDays($d)
    $weekend = ($day.DayOfWeek -eq [DayOfWeek]::Saturday) -or ($day.DayOfWeek -eq [DayOfWeek]::Sunday)
    $temp = 29 - 13 * $d / 44 + (U -2.5 2.5)
    $tariff = if ($day.Month -eq 9) { 'summer' } else { 'standard' }
    foreach ($b in $buildings) {
        $r = if ($weekend) { $b.we } else { $b.wd }
        $kwh = [math]::Round((U $r[0] $r[1]))
        $peak = if ($b.code -eq 'EM-D03') { $kwh / 24 * (U 1.05 1.15) } else { $kwh / 24 * (U 1.5 2.1) }
        $occ = if ($weekend) { I $b.occ[2] $b.occ[3] } else { I $b.occ[0] $b.occ[1] }
        if ($b.code -eq 'EM-A01' -and (D $day 'yyyy-MM-dd') -eq '2026-09-22') { $kwh = 24900; $peak = 2450 }
        $lines.Add((@($b.code, $b.name, (D $day 'yyyy-MM-dd'), $kwh, (F $peak '0.0'), (F $temp '0.0'), $occ, $tariff) -join "`t"))
    }
}
Save 'energy_meter_utf16.txt' (($lines -join "`r`n") + "`r`n") $utf16

# ---------- survey_responses.csv: comma-separated, UTF-8, LF ----------
# 120 NPS survey answers. Comments contain commas, quotes, line breaks and some Korean text (UTF-8 after the first 64 KiB
# would also be detected; here it is mixed in from the start). Planted: one answer that took 186 minutes (the tab was
# left open) and no support rating from people who never contacted support (empty).
$comments = @{
    pos = 'Fast and simple. The CSV import saved me hours.', 'Support answered in 5 minutes, great!',
    'Best tool we tried this year', 'Love the templates, my team uses them daily', '보고서 자동화가 정말 편해요'
    mid = 'Good overall, but the dashboard is slow on large files.', 'Missing an API for bulk updates',
    'Would like more export formats (Parquet, XLSX)', 'Pricing is fine for Pro, Team feels expensive',
    '모바일 화면이 조금 불편해요'
    neg = 'Pricing page said "Pro" but I was charged for Team.', "Two issues:`n1) login loops on Safari`n2) CSV import fails on files over 50 MB",
    'Too many emails, and unsubscribe does not work', 'Lost my filters after the update, very frustrating',
    '한글 파일명이 깨져서 업로드가 안 됩니다'
}
$answers = New-Object System.Collections.Generic.List[object]
$oct20 = [datetime]'2026-10-20'
for ($n = 0; $n -lt 120; $n++) {
    $plan = PickW @(@('Free', 40), @('Pro', 30), @('Team', 20), @('Enterprise', 10))
    switch ($plan) {
        'Free' { $nps = I 4 9 }
        'Pro' { $nps = I 6 10 }
        'Team' { $nps = I 7 10 }
        default { $nps = PickW @(@(3, 10), @(5, 10), @(8, 30), @(9, 25), @(10, 25)) }
    }
    $sentiment = if ($nps -ge 9) { 'pos' } elseif ($nps -ge 7) { 'mid' } else { 'neg' }
    $answers.Add([pscustomobject]@{
            at = $oct20.AddMinutes([math]::Floor((U 0 12) * 1440)); plan = $plan; nps = $nps
            ease = [int](Clamp ([math]::Round($nps / 2 + (U -1 1))) 1 5)
            support = $(if ($rng.NextDouble() -lt 0.15) { '' } else { [string][int](Clamp ([math]::Round($nps / 2 + (U -1.5 1))) 1 5) })
            recommend = $(if ($nps -ge 9) { 'yes' } elseif ($nps -ge 7) { 'maybe' } else { 'no' })
            minutes = F (U 2 9) '0.0'
            comment = $(if ($rng.NextDouble() -lt 0.3) { '' } else { Pick $comments[$sentiment] })
        })
}
$answers[77].minutes = '186.0'
$lines = New-Object System.Collections.Generic.List[string]
$lines.Add('response_id,submitted_at,plan,nps,ease_of_use,support_rating,would_recommend,minutes_to_complete,comment')
$n = 0
foreach ($a in ($answers | Sort-Object { $_.at })) {
    $n++
    $lines.Add((Row @(('R-{0:0000}' -f $n), (D $a.at 'yyyy-MM-dd HH:mm'), $a.plan, $a.nps, $a.ease, $a.support, $a.recommend,
                $a.minutes, $a.comment) ','))
}
Save 'survey_responses.csv' (($lines -join "`n") + "`n") $utf8

# ---------- customers_messy.csv: comma-separated, UTF-8, CRLF; a deliberately messy CRM export ----------
# 80 customers and 3 exact duplicate rows. Planted quality problems: inconsistent case and spaces, mixed date and phone
# formats, invalid or missing emails, an impossible age (212), "N/A" in a numeric column, a 45,000,000 KRW spend that
# is far above the rest, empty segments and mixed yes/no spellings.
$given = 'Minseo', 'Jihoon', 'Seoyeon', 'Doyun', 'Hayoon', 'Junwoo', 'Yerin', 'Siwoo', 'Jiwoo', 'Eunwoo', 'Subin',
'Hyunwoo', 'Chaewon', 'Minjun', 'Yuna', 'Taeyang'
$family = 'Kim', 'Lee', 'Park', 'Choi', 'Jung', 'Kang', 'Cho', 'Yoon', 'Jang', 'Lim'
$lines = New-Object System.Collections.Generic.List[string]
$lines.Add('customer_id,name,email,phone,city,signup_date,age,annual_spend_krw,segment,marketing_opt_in')
$signupFrom = [datetime]'2024-01-01'
for ($n = 0; $n -lt 80; $n++) {
    $g = Pick $given; $fam = Pick $family
    switch (PickW @(@('ok', 86), @('lower', 8), @('upper', 6))) {
        'lower' { $name = '  ' + $fam.ToLower() + ' ' + $g.ToLower() }
        'upper' { $name = ($fam + ' ' + $g).ToUpper() }
        default { $name = $fam + ' ' + $g }
    }
    if ($n -in 4, 19, 38, 60, 72) { $email = '' }
    elseif ($n -in 27, 66) { $email = $g.ToLower() + '.' + $fam.ToLower() + '@' }
    elseif ($n -eq 45) { $email = $g.ToLower() + ' at example.com' }
    else { $email = $g.ToLower() + '.' + $fam.ToLower() + $n + '@example.com' }
    $digits = '{0:0000}' -f (I 0 9999)
    $digits2 = '{0:0000}' -f (I 0 9999)
    switch (PickW @(@('dash', 60), @('plain', 20), @('intl', 12), @('none', 8))) {
        'dash' { $phone = "010-$digits-$digits2" }
        'plain' { $phone = "010$digits$digits2" }
        'intl' { $phone = "+82 10 $digits $digits2" }
        default { $phone = '' }
    }
    $city = PickW @(@('Seoul', 45), @(' seoul', 5), @('SEOUL', 4), @('Seoul ', 3), @('Busan', 15), @('busan', 4),
        @('Incheon', 12), @('Daegu', 8), @('Daejeon', 4))
    $signup = $signupFrom.AddDays((I 0 1034))
    $signupText = D $signup (PickW @(@('yyyy-MM-dd', 70), @('MM/dd/yyyy', 15), @('d MMM yyyy', 10), @('yyyy.MM.dd', 5)))
    $age = [string](I 19 68)
    if ($n -eq 23) { $age = '212' } elseif ($n -eq 51) { $age = '-1' } elseif ($n -in 8, 31, 47, 70) { $age = '' }
    $spend = [math]::Round([math]::Exp((U ([math]::Log(300000)) ([math]::Log(12000000)))) / 10000) * 10000
    $spendText = F $spend '#,##0'
    if ($n -in 12, 58) { $spendText = 'N/A' } elseif ($n -in 5, 36, 77) { $spendText = '' } elseif ($n -eq 64) {
        $spend = 45000000; $spendText = '45,000,000'
    }
    $segment = if ($spend -ge 4000000) { Pick 'VIP', 'VIP', 'VIP', 'VIP', 'vip' } elseif ($spend -ge 1000000) {
        Pick 'Regular', 'Regular', 'Regular', 'Regular', 'Regular', 'Regular', 'Regular', 'Regular', 'Regular', 'regular '
    } else { 'New' }
    if ($n -in 9, 21, 40, 55, 68, 79) { $segment = '' }
    $optIn = PickW @(@('Y', 40), @('N', 30), @('yes', 8), @('no', 7), @('TRUE', 5), @('', 10))
    $row = Row @(('CU-{0}' -f (1001 + $n)), $name, $email, $phone, $city, $signupText, $age, $spendText, $segment, $optIn) ','
    $lines.Add($row)
    if ($n -in 10, 33, 57) { $lines.Add($row) }
}
Save 'customers_messy.csv' (($lines -join "`r`n") + "`r`n") $utf8
