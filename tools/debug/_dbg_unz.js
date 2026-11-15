const fs=require('fs'),zlib=require('zlib');
const f='C:\\Users\\17277\\.dsh\\sessions\\--D-Projects-llm-router--\\session-5754fd79-ca26-4a28-a940-f12772ed42f3\\session.v4.jsonl.zstd';
const b=fs.readFileSync(f); const magic=Buffer.from([0x28,0xB5,0x2F,0xFD]);
let offs=[],i=0; while((i=b.indexOf(magic,i))!==-1){offs.push(i);i++;}
let out=''; for(let k=0;k<offs.length;k++){try{out+=zlib.zstdDecompressSync(b.subarray(offs[k])).toString('utf8')+'\n';}catch(e){}}
fs.writeFileSync('D:\\Projects\\llm-router\\logs\\_dbg_cur.jsonl',out);
const lines=out.split('\n').filter(l=>l.includes('request/header'));
const seen=lines.map(l=>{try{const j=JSON.parse(l);return j.seq+' '+j.time+' '+JSON.stringify(j.data.header.config);}catch(e){return 'ERR '+l.slice(0,100);}});
console.log(seen.slice(-6).join('\n'));
console.log('total frames',offs.length,'headers',lines.length);
