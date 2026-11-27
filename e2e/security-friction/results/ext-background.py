"""Run one bounded matrix phase and deliver a completion event."""
import json
import os
from pathlib import Path
import subprocess
import sys

root = Path(__file__).resolve().parents[1]
phase = sys.argv[1]
args = [sys.executable, str(root/'run.py')]
for prefix in ('tuic-','orch-','apple-','research-','knowledge-'):
    args += ['--id-prefix',prefix]
configs = ['yolo-off'] if phase == 'ext-calib' else ['yolo-off','yolo-adaptive','auto-adaptive','default-adaptive','auto-workspace','default-workspace']
for config in configs:
    args += ['--config',config]
extra = sys.argv[2:]
retry = '--retry' in extra
extra = [x for x in extra if x != '--retry']
args += ['--jobs','3','--batch',phase, *([] if retry else ['--skip-done']), *extra]
rc = subprocess.run(args).returncode
body = json.dumps({'message':f'{phase} finished rc={rc}; inspect results/{phase}/runs.jsonl and {phase}.log'}).encode()
socket = os.environ.get('TUIC_SOCKET')
if socket:
    subprocess.run(['curl','-sS','--fail-with-body','--unix-socket',socket,'--data-binary','@-','http://localhost/agent/webhook/a1e21e00-22b6-4983-9cac-aa7c164a0a41'],input=body)
