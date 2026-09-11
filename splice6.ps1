$f='F:\boruix-project\audiod\src\lib.rs'
$c=[System.IO.File]::ReadAllText($f)
$blk=[System.IO.File]::ReadAllText('F:\boruix-project\audiod\m4impl.txt')
$anchor='#[cfg(test)]'
$i=$c.IndexOf($anchor)
if($i -lt 0){Write-Output 'ANCHOR NOT FOUND';exit 1}
$c=$c.Substring(0,$i)+$blk+$c.Substring($i)
[System.IO.File]::WriteAllText($f,$c)
Write-Output 'spliced'