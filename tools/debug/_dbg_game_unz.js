const fs=require('fs'),zlib=require('zlib');
const f='C:\\Users\\17277\\.dsh\\sessions\\--D-Projects-Game--\\session-400d285c-0e09-4245-9320-e001f38e8847\\session.v4.jsonl.zstd';
const b=fs.readFileSync(f); console.log('mtime',fs.statSync(f).mtime.toISOString(),'size',b.length);
const magic=Buffer.from([0x28,0xB5,0x2F,0xFD]);
let offs=[],i=0; while((i=b.indexOf(magic,i))!==-1){offs.push(i);i++;}
let out=''; for(let k=0;k<offs.length;k++){try{out+=zlib.zstdDecompressSync(b.subarray(offs[k])).toString('utf8')+'\n';}catch(e){}}
fs.writeFileSync('D:\\Projects\\llm-router\\logs\\_dbg_game.jsonl',out);
console.log('frames',offs.length,'bytes',out.length);
for(const l of out.split('\n')){
  if(!l.startsWith('{"type":"request/header"')) continue;
  try{const j=JSON.parse(l); console.log('seq='+j.seq,'t='+j.time,JSON.stringify(j.data.header.config), 'tools='+(j.data.header.tools?j.data.header.tools.length:0));}catch(e){console.log('ERR',l.slice(0,80));}
}
