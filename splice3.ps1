$f='F:\boruix-project\audiod\src\lib.rs'
$c=[System.IO.File]::ReadAllText($f)
$state=[System.IO.File]::ReadAllText('F:\boruix-project\audiod\mixerstate.txt')
$tests=[System.IO.File]::ReadAllText('F:\boruix-project\audiod\m3_tests.txt')
$nl=[char]13+[char]10
$anchor='#[cfg(test)]'
$i=$c.IndexOf($anchor)
if($i -lt 0){Write-Output 'ANCHOR NOT FOUND';exit 1}
$c=$c.Substring(0,$i)+$state+$c.Substring($i)
# now insert tests before the closing brace of mod tests (last '}' in file)
$j=$c.LastIndexOf('}')
$c=$c.Substring(0,$j)+$tests+$nl+$c.Substring($j)
[System.IO.File]::WriteAllText($f,$c)
Write-Output 'spliced'