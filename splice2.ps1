$f='F:\boruix-project\audiod\src\lib.rs'
$c=[System.IO.File]::ReadAllText($f)
$blk=[System.IO.File]::ReadAllText('F:\boruix-project\audiod\m2_accept.txt')
$anchor='}'
$i=$c.LastIndexOf($anchor)
if($i -lt 0){Write-Output 'ANCHOR NOT FOUND';exit 1}
$c=$c.Substring(0,$i)+$blk+[char]13+[char]10+$c.Substring($i)
[System.IO.File]::WriteAllText($f,$c)
Write-Output 'spliced'