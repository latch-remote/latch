#!/usr/bin/env python3
"""Create a self-contained chart from the recorded native viewer counters."""
import json
import pathlib

root = pathlib.Path(__file__).resolve().parent
names = [('diagnostic-auto-noise-1080-60', 'Earlier controller'),
         ('controller2-auto-noise-1080-60', 'Updated controller')]
series = []
for name, label in names:
    path = root / (name + '.metrics.json')
    if path.exists():
        rows = json.loads(path.read_text())
        series.append({'name': label, 'rows': [row for row in rows if row['elapsed_s'] >= 5]})
page = '''<!doctype html><html><head><meta charset="utf-8"><style>
body{font-family:var(--font-sans);color:var(--foreground);margin:0}h2{font-size:18px;margin:0 0 8px}p{font-size:13px;color:var(--muted-foreground);line-height:1.5;margin:8px 0 16px}canvas{display:block;width:100%;height:235px}section{margin-bottom:20px}.legend{display:flex;gap:20px;font-size:13px;margin-bottom:12px}.legend span:before{content:"";display:inline-block;width:18px;height:3px;margin:0 7px 3px 0;background:var(--chart-1)}.legend span+span:before{background:var(--chart-2)}output{font-family:var(--font-mono);font-size:12px;display:block;min-height:20px;color:var(--muted-foreground)}
</style></head><body><h2>1080p60 synthetic field stream</h2>
<p>20 Mbps requested. Mac VideoToolbox encoding → Windows DXVA decoding. Optimized development builds. Values use observed frame counters and encoded-video bytes.</p>
<div class="legend"><span>Earlier controller</span><span>Updated controller</span></div>
<section><strong>Decoded frames per second</strong><canvas id="fps" aria-label="Decoded frame delivery over elapsed seconds"></canvas><output id="fps-value"></output></section>
<section><strong>Encoded video throughput, Mbps</strong><canvas id="mbps" aria-label="Encoded video throughput over elapsed seconds"></canvas><output id="mbps-value"></output></section>
<p>The initial five seconds are omitted. Rendering preparation and physical scanout have separate measurement limits described in REPORT.md. The updated-controller run includes a field-only UDP blackhole after startup.</p>
<script>const series=DATA;
function chart(id,key,maximum){
 const canvas=document.getElementById(id),ctx=canvas.getContext('2d'),output=document.getElementById(id+'-value');
 const css=getComputedStyle(document.documentElement),fg=css.getPropertyValue('--foreground').trim()||'#ddd',muted=css.getPropertyValue('--muted-foreground').trim()||'#888',border=css.getPropertyValue('--border').trim()||'#444';
 const colors=[css.getPropertyValue('--chart-1').trim()||'#f09952',css.getPropertyValue('--chart-2').trim()||'#51baca'];
 const width=canvas.clientWidth,height=235,ratio=devicePixelRatio||1;canvas.width=width*ratio;canvas.height=height*ratio;ctx.scale(ratio,ratio);
 const left=40,right=12,top=15,bottom=31,xMax=120,w=width-left-right,h=height-top-bottom;
 ctx.font='11px sans-serif';ctx.lineWidth=1;
 for(let value=0;value<=maximum;value+=maximum/4){const y=top+h-value/maximum*h;ctx.strokeStyle=border;ctx.beginPath();ctx.moveTo(left,y);ctx.lineTo(width-right,y);ctx.stroke();ctx.fillStyle=muted;ctx.textAlign='right';ctx.fillText(value.toFixed(value%1?1:0),left-8,y+4);}
 for(let second=0;second<=120;second+=30){const x=left+second/xMax*w;ctx.fillStyle=muted;ctx.textAlign='center';ctx.fillText(second+'s',x,height-9);}
 series.forEach((s,index)=>{ctx.strokeStyle=colors[index];ctx.lineWidth=2;ctx.beginPath();let first=true;for(const row of s.rows){const x=left+row.elapsed_s/xMax*w,y=top+h-Math.min(maximum,row[key]||0)/maximum*h;if(first){ctx.moveTo(x,y);first=false}else ctx.lineTo(x,y)}ctx.stroke();});
 canvas.onmousemove=event=>{const second=Math.max(0,Math.min(xMax,(event.offsetX-left)/w*xMax));output.textContent=series.map(s=>{const row=s.rows.reduce((a,b)=>Math.abs(a.elapsed_s-second)<Math.abs(b.elapsed_s-second)?a:b);return s.name+' '+row.elapsed_s.toFixed(1)+'s  '+(row[key]||0).toFixed(2)}).join(' · ');};
}
function draw(){chart('fps','decode_count_fps',80);chart('mbps','mbps',20)}window.addEventListener('resize',draw);draw();</script></body></html>'''
(root / 'comparison.html').write_text(page.replace('DATA', json.dumps(series)))
