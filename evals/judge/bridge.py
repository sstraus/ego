"""One bounded provider exchange for the Rust production-wire eval harness.

Keys are read from the process environment only. Response bodies and headers are
never logged. The Rust WireGenerator owns encoding and production SSE decoding.
"""
import hashlib
import json
import os
import sys
import time
import urllib.error
import urllib.request

request = json.load(sys.stdin)
provider = os.environ['EGO_JUDGE_MODEL'].split('/', 1)[0]
base, key_name = {
    # A local daemon: no key, no cost. Override the address with OLLAMA_BASE.
    'ollama': (os.environ.get('OLLAMA_BASE', 'http://127.0.0.1:11434'), None),
    'openrouter': ('https://openrouter.ai', 'OPENROUTER_API_KEY'),
    'openai': ('https://api.openai.com', 'OPENAI_API_KEY'),
    'anthropic': ('https://api.anthropic.com', 'ANTHROPIC_API_KEY'),
}[provider]
body = request['body']
if request.get('judge_event') is not None:
    event = request['judge_event']
    if provider not in ('ollama', 'openrouter') or event.get('kind') != 'JudgeResponse':
        raise ValueError('recorded replay requires a JudgeResponse and a chat-completions provider')
    record = event.get('input') or {}
    instructions, facts = record.get('instructions'), record.get('facts')
    if not isinstance(instructions, str) or not isinstance(facts, str):
        raise ValueError('recorded judge input lacks instructions or facts')
    if hashlib.sha256(instructions.encode()).hexdigest() != record.get('instructions_sha256'):
        raise ValueError('recorded judge instructions hash mismatch')
    json.loads(facts)
    body['messages'] = [
        {'role': 'system', 'content': instructions},
        {'role': 'user', 'content': '<untrusted_action_json>\n' + facts + '\n</untrusted_action_json>'},
    ]
if provider == 'openrouter':
    # Evaluation-only route preference; production mapping is unchanged.
    body['provider'] = {'sort': 'latency'}
    body['usage'] = {'include': True}
headers = {'Content-Type': 'application/json'}
if provider == 'anthropic':
    headers.update({'x-api-key': os.environ[key_name], 'anthropic-version': '2023-06-01'})
elif key_name:
    headers['Authorization'] = 'Bearer ' + os.environ[key_name]
error_code = None
error_parameter = None
error_category = None
started = time.monotonic()
try:
    req = urllib.request.Request(base + request['path'], json.dumps(body).encode(), headers, method=request['method'])
    with urllib.request.urlopen(req, timeout=20) as response:
        status = response.status
        raw = response.read(2 * 1024 * 1024 + 1)
        if len(raw) > 2 * 1024 * 1024:
            raise ValueError('response exceeded bound')
except urllib.error.HTTPError as error:
    # An error can echo credentials. Do not retain or print its body.
    status, raw = error.code, b''
    try:
        refusal = json.loads(error.read(16384)).get('error', {})
        message = str(refusal.get('message', '')).lower()
        error_category = next((term for term in ['model', 'max_tokens', 'max_completion_tokens', 'reasoning', 'thinking', 'credit', 'balance', 'permission', 'route', 'provider', 'unsupported'] if term in message), 'unclassified')
        code = refusal.get('code')
        parameter = refusal.get('param')
        error_code = code if isinstance(code, (int, float)) or isinstance(code, str) and code.replace('_', '').isalnum() and len(code) < 80 else None
        error_parameter = parameter if isinstance(parameter, str) and parameter.replace('_', '').replace('.', '').isalnum() and len(parameter) < 80 else None
    except Exception:
        pass
except Exception:
    print('provider transport failed (details suppressed)', file=sys.stderr)
    sys.exit(1)
metadata = {'status': status, 'provider': provider if provider != 'openrouter' else None, 'request_ms': (time.monotonic() - started) * 1000, 'error_code': error_code, 'error_parameter': error_parameter, 'error_category': error_category}
for line in raw.decode(errors='replace').splitlines():
    if not line.startswith('data: '):
        continue
    try:
        event = json.loads(line[6:])
    except ValueError:
        continue
    if isinstance(event, dict):
        if event.get('provider'):
            metadata['provider'] = event['provider']
        if event.get('id'):
            metadata['generation_id'] = event['id']
        usage = event.get('usage')
        if usage:
            metadata['raw_usage'] = usage
# Only the decoder sees SSE. Rust writes selected metadata and verdicts, not SSE.
json.dump({'status': status, 'sse': raw.decode(errors='replace'), 'metadata': metadata}, sys.stdout)
