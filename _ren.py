import io
p=r'F:\boruix-project\audiod\src\lib.rs'
s=io.open(p,encoding='utf-8').read()
s=s.replace('pub fn next(&mut self) -> f32 {','pub fn next_sample(&mut self) -> f32 {')
s=s.replace('let v = r.next();','let v = r.next_sample();')
s=s.replace('let after = r.next();','let after = r.next_sample();')
s=s.replace('last = r.next();','last = r.next_sample();')
io.open(p,'w',encoding='utf-8').write(s)
p2=r'F:\boruix-project\audiod\src\main.rs'
t=io.open(p2,encoding='utf-8').read()
t=t.replace('ramps[k].next()','ramps[k].next_sample()')
io.open(p2,'w',encoding='utf-8').write(t)
print('renamed')