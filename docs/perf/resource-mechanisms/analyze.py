"""Analyze complete matched hosted pairs; never substitute an unmatched median."""
import argparse,csv,json,math,random,statistics
from pathlib import Path
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("artifacts", type=Path, help="Directories grouped as mechanism/runner")
parser.add_argument("--manifest", type=Path, default=Path(__file__).with_name("runs.json"))
parser.add_argument("--output", type=Path, required=True)
args = parser.parse_args()
ROOT = args.artifacts
args.output.mkdir(parents=True, exist_ok=True)
runs = json.loads(args.manifest.read_text())
metrics=['size_kib','idle_retired_rss_kib','idle_rss_kib','idle_daemon_cpu_ms','cold_reply_ms','attach_p50_ms','attach_p99_ms','driver_cpu_ms','driver_regular_cpu_ms','driver_rss_kib','driver_regular_rss_kib','large_json_cpu_ms','large_json_regular_cpu_ms','large_json_rss_kib','large_json_regular_rss_kib','llm_decode_cpu_ms','startup_ms','startup_cpu_ms','driver_kernel_ms','driver_user_ms']
rows=[];summary=[]
def effect(a,b,relative):
 return [math.log(x/y) if relative else x-y for x,y in zip(a,b)]
def interval(values,relative):
 rng=random.Random(15015)
 samples=sorted(statistics.fmean(rng.choices(values,k=len(values))) for _ in range(5000))
 convert=(lambda n:math.expm1(n)*100) if relative else (lambda n:n)
 return tuple(convert(n) for n in [statistics.fmean(values),samples[124],samples[4874]])
for run in runs:
 path=ROOT/run['mechanism']/run['target']/'paired.json'
 if not path.exists() or not (path.parent/'.complete').exists():continue
 data=json.loads(path.read_text());assert data['candidate_revision']==run['sha'],run
 marker=json.loads((path.parent/'.complete').read_text());assert marker['run_id']==run['run_id'],run
 expected=run.get('reference', 'e2b56c644a0f5456c639225352cf219db1069230' if run['mechanism']=='sham' else '4b662d7ab8151802b8417e71378c8ec800834f34' if run['mechanism']=='full-vs-main' else '93344bb06d9096103ecf6ed18d2d78058a30791d')
 assert data['reference_revision']==expected,run
 assert data['completed_rounds']=={'reference':20,'candidate':20},run
 assert len(data['trials'])==20,run
 for i,t in enumerate(data['trials']):
  for role in ['reference','candidate']:
   idle_path=path.parent/'paired'/f'{i}-{role}'/'idle-0.json'
   if idle_path.exists():
    t[role]['idle_retired_rss_kib']=[json.loads(idle_path.read_text())['after'][0]['rss_kib']]
 on,off=('candidate','reference') if run['mechanism']=='full-vs-main' else ('reference','candidate')
 item={**run,'effects':{},'host':data['host']}
 for metric in metrics:
  if metric=='size_kib':
   a=[data['roles'][on][metric]['median']];b=[data['roles'][off][metric]['median']]
  elif any(metric not in t[on] or metric not in t[off] for t in data['trials']):continue
  else:
   a=[statistics.median(t[on][metric]) for t in data['trials']]
   b=[statistics.median(t[off][metric]) for t in data['trials']]
  relative=not(metric.endswith('rss_kib') or metric in ['idle_daemon_cpu_ms','size_kib']) and min(a+b)>0
  values=effect(a,b,relative);point,low,high=interval(values,relative)
  r=dict(mechanism=run['mechanism'],target=run['target'],run_id=run['run_id'],metric=metric,
         enabled_median=statistics.median(a),disabled_median=statistics.median(b),
         paired_delta=point,paired_median_delta=(math.expm1(statistics.median(values))*100 if relative else statistics.median(values)),ci_low=low,ci_high=high,unit='%' if relative else 'KiB' if metric.endswith('kib') else 'ms',
         pairs=len(a),enabled_rounds=json.dumps(a),disabled_rounds=json.dumps(b))
  if len(a)>1:
   leading=[i for i,order in enumerate(data['order']) if order[0]==on]
   trailing=[i for i,order in enumerate(data['order']) if order[0]!=on]
   r['enabled_first_delta']=interval([values[i] for i in leading],relative)[0]
   r['disabled_first_delta']=interval([values[i] for i in trailing],relative)[0]
  else:
   r['enabled_first_delta']=point;r['disabled_first_delta']=point
  rows.append(r);item['effects'][metric]=r
 if 'driver_cpu_ms' in item['effects'] and 'llm_decode_cpu_ms' in item['effects']:
  logs={m:effect(json.loads(item['effects'][m]['enabled_rounds']),json.loads(item['effects'][m]['disabled_rounds']),True) for m in ['driver_cpu_ms','llm_decode_cpu_ms']}
  try:item['driver_decode_paired_correlation']=statistics.correlation(logs['driver_cpu_ms'],logs['llm_decode_cpu_ms'])
  except statistics.StatisticsError:item['driver_decode_paired_correlation']=None
 summary.append(item)
 print(run['mechanism'],run['target'], {m:tuple(round(item['effects'][m][k],3) for k in ['paired_delta','ci_low','ci_high']) for m in ['idle_rss_kib','driver_cpu_ms','cold_reply_ms','attach_p99_ms','llm_decode_cpu_ms'] if m in item['effects']},flush=True)
if rows:
 with (args.output/'effects.csv').open('w') as f:
  writer=csv.DictWriter(f,fieldnames=list(rows[0]),lineterminator='\n');writer.writeheader();writer.writerows(rows)
 (args.output/'effects.json').write_text(json.dumps(summary,indent=2)+'\n')
print('complete analyses',len(summary),'of',len(runs))
