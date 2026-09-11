$f='F:\boruix-project\audiod\src\main.rs'
$c=[System.IO.File]::ReadAllText($f)
$blk=[System.IO.File]::ReadAllText('F:\boruix-project\audiod\main_new.txt')
$anchor='pub extern "C" fn user_main'
$i=$c.IndexOf($anchor)
if($i -lt 0){Write-Output 'ANCHOR NOT FOUND';exit 1}
$c=$c.Substring(0,$i)+$blk+[char]13+[char]10
[System.IO.File]::WriteAllText($f,$c)
Write-Output ('spliced, lines=' + ($c -split [char]10).Count)