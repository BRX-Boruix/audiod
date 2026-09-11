$out='F:\boruix-project\audiod\m4.txt'
$a=[System.IO.File]::ReadAllText('F:\boruix-project\audiod\m4a.txt')
$b=[System.IO.File]::ReadAllText('F:\boruix-project\audiod\m4b.txt')
$c=[System.IO.File]::ReadAllText('F:\boruix-project\audiod\m4c.txt')
$nl=[char]13+[char]10
[System.IO.File]::WriteAllText($out, $a+$nl+$b+$nl+$c)
$f='F:\boruix-project\audiod\src\lib.rs'
$src=[System.IO.File]::ReadAllText($f)
$blk=[System.IO.File]::ReadAllText($out)
$j=$src.LastIndexOf('}')
$src=$src.Substring(0,$j)+$blk+$nl+$src.Substring($j)
[System.IO.File]::WriteAllText($f,$src)
Write-Output 'spliced'