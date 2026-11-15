const fs=require('fs');
const lines=fs.readFileSync('_dbg_game.jsonl','utf8').split('\n').filter(Boolean);
console.log('total',lines.length);
for(const l of lines){
  let j; try{j=JSON.parse(l)}catch(e){continue}
  const t=j.time;
  if(t && t>=1794725220000-1){
    const d=j.data||{};
    let extra='';
    if(j.type==='request/header') extra=JSON.stringify(d.header.config);
    if(j.type==='assistant/attempt') extra='turn='+d.turn+' step='+d.step+' status='+JSON.stringify((d.stream||[]).map(s=>s.chunk&&s.chunk.type));
    if(j.type==='turn/end') extra=JSON.stringify(d.reason&&d.reason.kind||d.reason).slice(0,120);
    if(j.type==='user/message') extra=(JSON.stringify(d.content)||'').slice(0,80);
    console.log(j.seq, j.type, t, extra.slice(0,200));
  }
}
