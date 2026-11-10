# Builds ui/catalog.js from the Ferdium recipe repository.
#
#   recipes:  https://github.com/ferdium/ferdium-recipes
#   source:   $env:TEMP\ferdium-recipes\recipes\<id>\package.json + icon.svg
#
# Every recipe becomes one catalog entry. Three shapes exist upstream, and all
# three are emitted instead of skipped:
#
#   1. plain        serviceURL is a real URL                      -> u = url
#   2. templated    serviceURL contains {placeholder}             -> u = template,
#                                                                   ph = [{n,k}]
#   3. self-hosted  hasCustomUrl with no serviceURL at all        -> cu = true
#
# The UI asks for the missing piece (a workspace slug, a server address, or the
# whole URL) when the tile is clicked, so a recipe that cannot be opened as-is
# still shows up in the catalog with its name and icon.
#
# Icons: the recipe's icon.svg is copied into ui/icons/<id>.svg and referenced by
# a relative path, so the catalog bundle stays small and icons load lazily.
# Recipes whose SVG is enormous (a 1024px PNG pasted into <image href="data:...">)
# are rasterised to a 160px PNG by rasterize-icons.mjs instead of being dropped.
#
# Output: ui/catalog.js  ->  window.RUSTBOX_CATALOG = [{section, apps:[...]}]
param(
  [string]$RecipesDir = "$env:TEMP\ferdium-recipes\recipes",
  [string]$OutFile = "$PSScriptRoot\..\ui\catalog.js",
  [string]$IconDir = "$PSScriptRoot\..\ui\icons",
  # Staging area for SVGs that are too big to ship as vectors. Emptied each run.
  [string]$IconStagingDir = "$PSScriptRoot\..\ui\icons-src",
  # Above this size an icon is rasterised to PNG rather than shipped as SVG.
  [int]$MaxIconBytes = 65536,
  # Icons are copied verbatim from the recipes repo, which means wildly
  # different ink coverage. normalize-icons.mjs rewrites each viewBox so the
  # glyph fills the same 78% of a square canvas; without it the catalog grid
  # looks uneven. Opt out with -SkipNormalize.
  [switch]$SkipNormalize
)

$ErrorActionPreference = 'Stop'
if (-not (Test-Path $RecipesDir)) { throw "recipes dir not found: $RecipesDir" }

# Section per recipe id. Anything not listed lands in "Other".
$sectionOf = @{}
function Add-Section([string]$section, [string[]]$ids) {
  foreach ($id in $ids) { $script:sectionOf[$id] = $section }
}
Add-Section 'Communication' @(
  'whatsapp','telegram','discord','slack','messenger','signal','skype','msteams','zoom',
  'google-meet','hangouts','wechat','line','viber','element','matrix','rocketchat',
  'mattermost','zulip','revolt','teamspeak','threema','wire','jitsi','whereby','mirotalk-webrtc',
  'twist','flock','chanty','highrise','kakaotalk','kik','groupme','irccloud','matrix-app'
)
Add-Section 'Email' @(
  'gmail','outlook','proton-mail','yahoo-mail','zoho-mail','fastmail','tutanota','mailfence',
  'roundcube','gmx','mailbox-org','thunderbird','spike','missive','mailspring','aol-mail',
  'icloud-mail','yandex-mail','epicmail','mailru','hotmail'
)
Add-Section 'AI' @(
  'chatgpt','claude','google-gemini','perplexity','poe','huggingchat','you','phind','copilot',
  'mistral','deepseek','grok','chatllm','open-webui','character-ai','pi','meta-ai','lmstudio',
  'jan','ollama-webui','librechat','typingmind'
)
Add-Section 'Productivity' @(
  'notion','evernote','onenote','todoist','ticktick','things','asana','trello','clickup',
  'monday','jira','confluence','basecamp','wrike','notion-calendar','google-calendar',
  'google-drive','google-docs','google-sheets','google-slides','google-keep','google-tasks',
  'google-contacts','google-photos','google-forms','dropbox','onedrive','box','nextcloud',
  'obsidian','roam-research','logseq','coda','airtable','smartsheet','quip','craft','bear',
  'slite','workflowy','remember-the-milk','rescuetime','clockify','toggl','harvest','todoist-calendar'
)
Add-Section 'Developer' @(
  'github','gitlab','bitbucket','azure-devops','gitea','gitkraken','sentry','linear',
  'sonarqube','vercel','netlify','heroku','digitalocean','cloudflare','aws','amazon-web-services',
  'stackoverflow','devdocs','codesandbox','codepen','replit','glitch','postman','insomnia',
  'jenkins','circleci','travis-ci','npm','docker-hub','grafana','datadog','new-relic',
  'it-tools','jsfiddle','sqlfiddle','regex101','carbon','ray-so','jsonhero'
)
Add-Section 'Social' @(
  'x','twitter','facebook','instagram','linkedin','reddit','tiktok','youtube','twitch',
  'mastodon','bluesky','threads','pinterest','tumblr','mewe','vk','odnoklassniki','mastodeck',
  'tweetdeck','wafrn','lemmy','kbin','parler','gab','rumble','odysee','peer-tube','patreon',
  'substack','medium','devto','hashnode','hackernews','lobsters','product-hunt','discourse'
)
Add-Section 'News' @(
  'feedly','inoreader','newsblur','the-old-reader','freshrss','miniflux','flipboard','pocket',
  'instapaper','readwise','raindrop','hackernews','techmeme','ground-news'
)
Add-Section 'Media' @(
  'spotify','soundcloud','deezer','tidal','apple-music','pandora','lastfm','bandcamp',
  'youtube-music','plex','jellyfin','netflix','prime-video','disney-plus','hbomax','hulu',
  'twitch','vlc','noisli','pocketcasts','overcast','castbox','audiobookshelf','navidrome'
)
Add-Section 'Office' @(
  'office365','microsoft-office','excel-online','word-online','powerpoint-online',
  'onlyoffice','nextcloud-office','collabora','zoho','wps-office','pdf-tools','ilovepdf'
)
Add-Section 'Finance' @(
  'paypal','wise','revolut','stripe','coinbase','binance','kraken','tradingview','yahoo-finance',
  'morningstar','nordnet','bux','robinhood','finance','mint','ynab','gnucash','invoice-ninja',
  'firefly'
)
Add-Section 'Design' @(
  'figma','canva','sketch','invision','miro','mural','excalidraw','drawio','lucidchart',
  'photopea','adobe-creative-cloud','dribbble','behance','unsplash','pexels','removebg'
)
Add-Section 'Learning' @(
  'coursera','udemy','edx','khan-academy','duolingo','brilliant','skillshare','pluralsight',
  'moodle','canvas','blackboard','wikipedia','wiktionary','anki','quizlet','deep-l','deepl',
  'google-translate','it-tools'
)
Add-Section 'Self-hosted' @(
  'home-assistant','gotify','memos','monicahq','paperless-ngx','paperless','unraid',
  'proxmox','portainer','homebridge','tandoor','mealie','actual-budget','vaultwarden',
  'bitwarden','1password','lastpass','dashy','heimdall','organizr','immich','photoprism',
  'ntfy','wakapi','wakatime','wallabag','nextcloud','open-webui','lobe-chat','jsoncrack',
  'penpot','netatmo-energy','traccar','tt-rss','trilium','synology-chat'
)
# Second pass: apps that landed in "Other" but clearly belong somewhere. Grouped
# by what the service actually is, not by vendor.
Add-Section 'Communication' @(
  'airmessage','bluebubbles','chatwork','cliq','dingtalk','fleep','flowdock','gitter',
  'glowing-bear','guilded','icq','idobata','igap','iris-messenger','keybase.io','kiwiirc',
  'lineworks','kaizala','xmpp','stoat','tchap','fluffychat','cinny','elk','phanpy','photon',
  'rubika','soroushplus','bale','eitaa','gap','bip','zalo','voxer','mightytext','mysms',
  'pulse-sms','pushbullet','air-droid','alibaba-chat','chatwoot','chatra','chatwithgpt',
  'dialpad','gotomeeting','goto','ring-central','webex-teams','sococo','gather','gather-v2',
  'bigbluebutton','nextdoor','slowly','badoo','tinder','sympatia','romeo','meetup',
  'snapchat','snapdrop','steamchat','gadugadu','circuit','ciscospark','stride','cliq',
  'hangoutschat','google-duo','google-voice','google-groups','max','virasty','wisgoon',
  'nazdika','fleep','parro','bycs-messenger','mirotalk-c2c','mirotalk-bro','mirotalk-p2p','mirotalk-sfu'
)
Add-Section 'Email' @(
  'anonaddy','hey','mailbox.org','office365-owa','onmail','posteo','purelymail','riseup',
  'simplelogin','skiff-mail','spikenow','temp-mail','thundermail','infomaniak-mail',
  'notion-mail','proton-pass','icloud','zoho-mail','tuta','tutanota'
)
Add-Section 'AI' @(
  'bard','chatwithgpt','deepai','duckduckgo-ai-chat','hugging-chat','lobe-chat','claude-ai',
  'primal','poe','copilot'
)
Add-Section 'Productivity' @(
  'anydo','clockwise','conceptboard','grammarly','hackmd','habitica','iCloud-reminders',
  'icloud-reminders','infomaniak-calendar','infomaniak-kdrive','ms-planner','mstodo',
  'notion-mail','paymo','plan','pomodoro-tracker','productive','reclaim','rydoo','scrumpy',
  'sendtask','simplenote','standardnotes','super-productivity','superlist','taiga',
  'teamweek','tick','weekplan','whimsical','zoho-projects','zeplin','pivotal-tracker',
  'pipefy','podio','teamleader','outreach','hubstaff','buffer','bring','good-reads',
  'fiverr','upwork','peopleperhour','truelancer','guru','ko-fi','gather','odoo',
  'proton-calendar','proton-drive','google-classroom','google-presentation',
  'google-spreadsheets','google-maps','ovhcloud','OVHCloud','iCloud'
)
Add-Section 'Developer' @(
  'azure-portal','bugzilla','crowdin','dev-community','devRant','dockerhub','exploit-db',
  'hastebin','jsoncrack','localazy','pastebin','stackexchange','stackoverflow-chat',
  'wakatime','wakapi','hacker-news','github','gitlab','bitbucket','jsoncrack','postman'
)
Add-Section 'Social' @(
  '80lv','aparat','brax.me','cara','campuswire','daily-dev','dev-community','devRant',
  'disqus','fandom','furaffinity','gettr','good-reads','instagram-direct-messages',
  'ko-fi','nomadlist','nostr','pixelfed','plurk','primal','producthunt','quora','reddit-chat',
  'ritetag','statuzer','strava','trakt','twitter-dm','virgool','xing','deviantart','meetup',
  'nextdoor','tinder','badoo','elk','phanpy','photon','pixelfed','peertube','peertube'
)
Add-Section 'News' @(
  'farsnews','google-news','hacker-news','techmeme','feedbin','tt-rss','wallabag','good-reads'
)
Add-Section 'Media' @(
  'epicgames','online-go','youtubemusic','trakt','80lv','fandom','aparat','deezer',
  'prime-video','plex','jellyfin','noisli','pocketcasts'
)
Add-Section 'Office' @(
  'odoo','office365-owa','ms-planner','mstodo','zoho-projects','cliq','ovhcloud'
)
Add-Section 'Finance' @(
  'monarch-money','paymo','invoice-ninja','coinbase','binance'
)
Add-Section 'Design' @(
  'penpot','zeplin','whimsical','conceptboard','diagrams','excalidraw','drawio','canva',
  'figma','miro','pixieset','jsoncrack'
)
Add-Section 'Learning' @(
  'blinkist','dict-cc','google-classroom','oreilly-learning','campuswire','sharezone',
  'somtoday','duolingo','wikipedia','moodle','canvas','deepai','deepl','google-translate',
  'it-tools','quizlet'
)
Add-Section 'Jobs' @(
  'divar','eestekhdam','iranestekhdam','irantalent','jobinja','jobvision','karboom','shad',
  'sheypoor','jollor','magic-level','nomadlist','upwork','fiverr','truelancer','guru',
  'peopleperhour'
)
Add-Section 'Iranian' @(
  'aparat','bale','divar','eestekhdam','eitaa','farsnews','igap','iranestekhdam','irantalent',
  'jobinja','jobvision','karboom','nazdika','rubika','shad','sheypoor','soroushplus',
  'virasty','virgool','wisgoon','gap','bip','splus'
)

$featured = @()
$featPath = Join-Path (Split-Path $RecipesDir -Parent) 'featured.json'
if (Test-Path $featPath) { $featured = @(Get-Content $featPath -Raw | ConvertFrom-Json) }

# Labels for the `{placeholder}` values. Derived from where the placeholder sits
# in the URL, because "teamId" means three different things upstream: a Slack
# subdomain, a Jira subdomain, a Twitch channel, a Zenhub workspace id. A single
# generic label would be wrong for most of them.
function Get-PlaceholderLabel([string]$url, [string]$key) {
  $esc = [regex]::Escape($key)
  # The whole URL is the placeholder: nothing can be inferred, ask for a URL.
  if ($url -eq "{$key}") { return 'Server URL' }
  # https://{key}/ — a host, so the user supplies the host only.
  if ($url -match "^https?://\{$esc\}/?$") { return 'Server host' }
  # https://{key}.example.com — a subdomain slug.
  if ($url -match "^https?://\{$esc\}\.") { return 'Workspace name' }
  # https://example.{key} — a domain suffix (scribens.com / scribens.fr).
  if ($url -match "\.\{$esc\}(/|$)") { return 'Domain suffix' }
  # Anywhere else: a path segment.
  return 'Workspace name'
}

# Where the derived label would still be vague, say what the service calls it.
# Only for recipes whose placeholder is a path segment with a specific meaning.
$placeholderLabel = @{
  'twitch'       = 'Channel name'
  'planitpoker'  = 'Room id'
  'zenhub'       = 'Workspace id'
  'facebookpages'= 'Page name'
  'whereby'      = 'Room name'
  'clubhouse'    = 'Room slug'
  'azure-devops' = 'Organisation name'
}

# Recipes with no serviceURL at all: they are self-hosted, so the user has to
# supply the address of their own instance. `hasCustomUrl` is the upstream flag
# that says exactly this, which is why it is used instead of a hardcoded list.
$customUrlIds = @{}
foreach ($dir in Get-ChildItem $RecipesDir -Directory) {
  $pkgPath = Join-Path $dir.FullName 'package.json'
  if (-not (Test-Path $pkgPath)) { continue }
  $pkg = Get-Content $pkgPath -Raw | ConvertFrom-Json
  $url = $null
  if ($pkg.config -and $pkg.config.serviceURL) { $url = [string]$pkg.config.serviceURL }
  if (-not $url -and $pkg.config -and $pkg.config.hasCustomUrl -eq $true) {
    $customUrlIds[$dir.Name] = $true
  }
}

$skipped = New-Object System.Collections.ArrayList
$rasterized = New-Object System.Collections.ArrayList
$templated = 0
$selfHosted = 0
$groups = @{}
$total = 0

# Staging directory is rebuilt from scratch: a stale SVG from a previous run
# would be rasterised again and silently resurrect a removed icon.
if (Test-Path $IconStagingDir) { Remove-Item $IconStagingDir -Recurse -Force }
New-Item -ItemType Directory -Path $IconStagingDir -Force | Out-Null
if (-not (Test-Path $IconDir)) { New-Item -ItemType Directory -Path $IconDir -Force | Out-Null }

foreach ($dir in Get-ChildItem $RecipesDir -Directory | Sort-Object Name) {
  $pkgPath = Join-Path $dir.FullName 'package.json'
  if (-not (Test-Path $pkgPath)) { continue }
  $pkg = Get-Content $pkgPath -Raw | ConvertFrom-Json

  $id = [string]$pkg.id
  if (-not $id) { $id = $dir.Name }
  $name = [string]$pkg.name
  if (-not $name) {
    # A recipe with no name cannot be rendered at all: there is nothing to
    # label the tile with. This is a genuinely broken upstream file.
    [void]$skipped.Add("$id (no name)")
    continue
  }

  $url = $null
  if ($pkg.config -and $pkg.config.serviceURL) { $url = [string]$pkg.config.serviceURL }

  $placeholders = @()
  $isSelfHosted = $false
  if (-not $url) {
    if ($customUrlIds.ContainsKey($id)) {
      # Self-hosted: the recipe ships no address at all, so the user supplies
      # one. `cu` alone drives the form field; there is no template to fill.
      $isSelfHosted = $true
      $url = ''
      $selfHosted++
    } else {
      # Neither a URL nor the hasCustomUrl flag: nothing to open.
      [void]$skipped.Add("$id (no URL and no hasCustomUrl)")
      continue
    }
  } elseif ($url -match '\{') {
    $keys = [regex]::Matches($url, '\{([^}]+)\}') | ForEach-Object { $_.Groups[1].Value } | Sort-Object -Unique
    foreach ($k in $keys) {
      $label = $placeholderLabel[$id]
      if (-not $label) { $label = Get-PlaceholderLabel $url $k }
      $placeholders += [pscustomobject]@{ n = $label; k = $k }
    }
    $templated++
  } elseif ($url -notmatch '^https?://[^\s/]+\.[^\s/]+') {
    # A serviceURL that is neither templated nor a usable absolute URL is broken
    # upstream. Keep the tile (the user can still add it by hand) but say so.
    [void]$skipped.Add("$id (unusable URL: $url)")
    continue
  }

  $icon = $null
  $svgPath = Join-Path $dir.FullName 'icon.svg'
  if (Test-Path $svgPath) {
    $len = (Get-Item $svgPath).Length
    $raw = [System.IO.File]::ReadAllText($svgPath)
    # Strip XML prolog/comments and collapse inter-tag whitespace.
    $raw = [regex]::Replace($raw, '<\?xml.*?\?>', '')
    $raw = [regex]::Replace($raw, '(?s)<!--.*?-->', '')
    $raw = [regex]::Replace($raw, '>\s+<', '><').Trim()
    if ($len -le $MaxIconBytes) {
      [System.IO.File]::WriteAllText((Join-Path $IconDir "$id.svg"), $raw, (New-Object System.Text.UTF8Encoding($false)))
      $icon = "icons/$id.svg"
    } else {
      # Too big to ship as a vector. Stage it for rasterisation; the PNG is
      # referenced instead, which keeps the artwork at a sane size.
      [System.IO.File]::WriteAllText((Join-Path $IconStagingDir "$id.svg"), $raw, (New-Object System.Text.UTF8Encoding($false)))
      $icon = "icons/$id.png"
      [void]$rasterized.Add($id)
    }
  }

  $section = 'Other'
  # Case-insensitive match: upstream recipe ids are inconsistently cased
  # (`TickTick`, `OVHCloud`, `iCloud`, `NewsBlur`).
  foreach ($key in $sectionOf.Keys) {
    if ([string]::Equals($key, $id, 'OrdinalIgnoreCase')) { $section = $sectionOf[$key]; break }
  }

  $entry = [pscustomobject]@{
    id      = $id
    n       = $name
    u       = $url
    i       = $icon
    feat    = ($featured -contains $id)
    section = $section
  }
  # Only present when the URL cannot be opened as-is, so the common case stays
  # three fields per entry instead of four.
  if ($placeholders.Count -gt 0) {
    $entry | Add-Member -NotePropertyName ph -NotePropertyValue $placeholders
  }
  if ($isSelfHosted) {
    $entry | Add-Member -NotePropertyName cu -NotePropertyValue $true
  }

  if (-not $groups.ContainsKey($section)) { $groups[$section] = New-Object System.Collections.ArrayList }
  [void]$groups[$section].Add($entry)
  $total++
}

$order = @('Featured','Communication','Email','AI','Productivity','Developer','Social','News',
           'Media','Office','Finance','Design','Learning','Self-hosted','Jobs','Iranian','Other')

$out = New-Object System.Collections.ArrayList
foreach ($section in $order) {
  if ($section -eq 'Featured') {
    $featEntries = @($groups.Values | ForEach-Object { $_ } | Where-Object { $_.feat } | Sort-Object n)
    if ($featEntries.Count -gt 0) { [void]$out.Add([pscustomobject]@{ section = 'Featured'; apps = $featEntries }) }
    continue
  }
  if (-not $groups.ContainsKey($section)) { continue }
  [void]$out.Add([pscustomobject]@{ section = $section; apps = @($groups[$section] | Sort-Object n) })
}
# Safety net: never silently drop a section that the order list does not know.
foreach ($section in ($groups.Keys | Sort-Object)) {
  if ($order -contains $section) { continue }
  [void]$out.Add([pscustomobject]@{ section = $section; apps = @($groups[$section] | Sort-Object n) })
}

$json = $out | ConvertTo-Json -Depth 6 -Compress
$stamp = (Get-Date).ToString('yyyy-MM-dd')
$header = @"
// GENERATED FILE - do not edit by hand.
// Source: https://github.com/ferdium/ferdium-recipes (recipes/*/package.json + icon.svg)
// Regenerate: pwsh -File scripts/build-catalog.ps1
// Generated: $stamp
// Recipes: $total  (templated: $templated, self-hosted: $selfHosted, skipped: $($skipped.Count))
window.RUSTBOX_CATALOG = $json;
"@
$OutDir = Split-Path $OutFile -Parent
if (-not (Test-Path $OutDir)) { New-Item -ItemType Directory -Path $OutDir | Out-Null }
[System.IO.File]::WriteAllText($OutFile, $header, (New-Object System.Text.UTF8Encoding($false)))

"apps=$total templated=$templated selfHosted=$selfHosted skipped=$($skipped.Count) sections=$($out.Count)"
"out=$((Get-Item $OutFile).Length) bytes"
if ($skipped.Count) { "skipped: $($skipped -join ', ')" }

$node = Get-Command node -ErrorAction SilentlyContinue

# --- Oversized icon pass ------------------------------------------------------
# 33 recipes ship a 1024px PNG inside an SVG, which is 17.5 MB in total and
# pointless at a 48px tile. Rasterising them to a 160px PNG keeps the artwork
# and costs ~15 KB each. Runs before normalization because the normalizer only
# reads .svg files, and a PNG has no viewBox to rewrite.
if ($rasterized.Count -gt 0) {
  $rasterizer = Join-Path $PSScriptRoot 'rasterize-icons.mjs'
  if (-not (Test-Path $rasterizer)) {
    throw "rasterize-icons.mjs missing but $($rasterized.Count) icons need it."
  }
  if (-not $node) {
    throw "$($rasterized.Count) icons need rasterizing but node is not on PATH."
  }
  Write-Host "rasterizing $($rasterized.Count) oversized icons..."
  & $node.Source $rasterizer $IconStagingDir $IconDir
  if ($LASTEXITCODE -ne 0) {
    throw "rasterize-icons.mjs failed (exit $LASTEXITCODE)."
  }
}

# --- Icon geometry pass -------------------------------------------------------
# This script overwrites ui/icons wholesale, so the normalization done by
# normalize-icons.mjs is lost on every regeneration. Run it here unless asked
# not to, otherwise the catalog silently reverts to uneven icons.
if (-not $SkipNormalize) {
  $normalizer = Join-Path $PSScriptRoot 'normalize-icons.mjs'
  if (-not (Test-Path $normalizer)) {
    Write-Warning "normalize-icons.mjs not found - icons left unnormalized."
  } else {
    if (-not $node) {
      Write-Warning "node not on PATH - icons left unnormalized. Run: node scripts/normalize-icons.mjs"
    } else {
      Write-Host "normalizing icons..."
      & $node.Source $normalizer
      if ($LASTEXITCODE -ne 0) {
        throw "normalize-icons.mjs failed (exit $LASTEXITCODE); icons may be inconsistent."
      }
    }
  }
}
