$f='F:\boruix-project\audiod\src\lib.rs'
$c=[System.IO.File]::ReadAllText($f)
$blk=[System.IO.File]::ReadAllText('F:\boruix-project\audiod\m3b.txt')
$j=$c.LastIndexOf('}')
if($j -lt 0){Write-Output 'ANCHOR NOT FOUND';exit 1}
$c=$c.Substring(0,$j)+$blk+[char]13+[char]10+$c.Substring($j)
[System.IO.File]::WriteAllText($f,$c)
Write-Output 'spliced'