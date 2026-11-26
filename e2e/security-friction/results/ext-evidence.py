"""Extract complete tool failure text for the selected matrix sessions only."""
import json
import os
from pathlib import Path
import sys

root = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(root))
import aggregate

runs = aggregate.load([root/'results/ext-calib', root/'results/ext-full'])
sessions = Path(os.environ.get('EGO_HOME', Path.home()/'.ego'))/'sessions'
home = str(Path.home())


def texts(value):
    if isinstance(value, dict):
        for k,v in value.items():
            if k in ('text','message','stderr','reason') and isinstance(v,str):
                yield v.replace(home,'~')
            elif isinstance(v,(dict,list)):
                yield from texts(v)
    elif isinstance(value,list):
        for v in value:
            yield from texts(v)


out = []
for r in runs.values():
    if r['outcome'] in ('allowed','judge') and r['expect'] != 'deny':
        continue
    snippets = []
    sid = r.get('session')
    path = sessions/str(sid)/'events.jsonl'
    if sid and path.exists():
        for line in path.open():
            if 'ToolCallCompleted' in line or 'PermissionDecided' in line:
                for t in texts(json.loads(line)):
                    if t not in snippets:
                        snippets.append(t)
    out.append({k:r.get(k) for k in ('scenario','config','expect','frequency','outcome','session','denied','denied_by')} |
               {'result':r['result'].replace(home,'~'),'tool_text':snippets})
(root/'results/ext-evidence.json').write_text(json.dumps(out,indent=2)+'\n')
print('evidence records',len(out))
