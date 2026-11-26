"""One coordinator-authorized continuation; no edits to the reusable runner."""
import hashlib, importlib.util, inspect, json, os, shutil, subprocess, sys, textwrap, time
from pathlib import Path
os.umask(0o077)
root=Path('/Users/stefano.straus/Gits/personal/ego__wt/chore-419-conformance-b7')
base=Path('/Users/stefano.straus/Gits/.tmp/ego-419-b7')
work=base/'final-continuation'
work.mkdir(exist_ok=True)
os.chdir(root)
spec=importlib.util.spec_from_file_location('conformance',root/'scripts/live-conformance.py')
m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
WIRE_MODEL='anthropic/anthropic/claude-haiku-5.5'
WIRE_HOME=work/'wire-home'
WIRE_HOME.mkdir(exist_ok=True)
home=Path('/Users/stefano.straus/Gits/.tmp/ego-419-continuation/operator-home')
# Derive the wire row from the same verified public Haiku facts, not invented rates.
content=(base/'models.toml').read_text()
haiku=content[content.index('[model."openrouter/anthropic/claude-haiku-5.5".capabilities]'):]
haiku=haiku.replace('openrouter/anthropic/claude-haiku-5.5',WIRE_MODEL)
# The actual mapping has portable Anthropic thinking blocks and short prompt-cache support.
haiku=haiku.replace('reasoning_carryover = "unsupported"','reasoning_carryover = "portable"').replace('cache = "none"','cache = "short"')
(WIRE_HOME/'models.toml').write_text(haiku)
# Preserve the exact Runner.invoke implementation except the explicit task adaptations:
# funded Anthropic wire through OpenRouter, and the diagnosed routed-Luna deadline.
source=textwrap.dedent(inspect.getsource(m.Runner.invoke))
old='    provider = model.split("/", 1)[0]\n'
assert source.count(old)==1
source=source.replace(old,'    wire_haiku = model == WIRE_MODEL\n    provider = "openrouter" if wire_haiku else model.split("/", 1)[0]\n')
old='    if provider == "openrouter" and getattr(self.args, "operator_home", None):\n'
assert source.count(old)==1
source=source.replace(old,'    if wire_haiku:\n        env["ANTHROPIC_API_KEY"] = env.pop("OPENROUTER_API_KEY")\n        env["ANTHROPIC_BASE_URL"] = "https://openrouter.ai/api"\n        env["EGO_HOME"] = str(WIRE_HOME)\n    if provider == "openrouter" and not wire_haiku and getattr(self.args, "operator_home", None):\n')
assert source.count('"--timeout", "50s"')==1
source=source.replace('"--timeout", "50s"','"--timeout", "120s" if model == "openrouter/openai/gpt-6-luna" else "50s"')
assert source.count('timeout=65')==1
source=source.replace('timeout=65','timeout=135 if model == "openrouter/openai/gpt-6-luna" else 65')
old='env[{"openai": "OPENAI_BASE_URL", "anthropic": "ANTHROPIC_BASE_URL"}[provider]] = proxy'
assert source.count(old)==1
source=source.replace(old,'env[{"openai": "OPENAI_BASE_URL", "anthropic": "ANTHROPIC_BASE_URL"}["anthropic" if wire_haiku else provider]] = proxy')
(work/'invoke-continuation.py').write_text(source)
namespace=dict(m.__dict__,WIRE_MODEL=WIRE_MODEL,WIRE_HOME=WIRE_HOME)
exec(compile(source,str(work/'invoke-continuation.py'),'exec'),namespace)
adapted=namespace['invoke']
# Missing isolated Codex credentials are a recorded prerequisite, never a paid retry/login.
skills_source=textwrap.dedent(inspect.getsource(m.Runner.skill_fixtures))
old='    for case, oracle in oracles.items():\n'
assert skills_source.count(old)==1
skills_source=skills_source.replace(old,old+'        if case == "model":\n            self.cases.append({"model":model,"provider":"openrouter","suite":"skills","case":"model","status":"blocked","reason":"isolated_home_missing_codex_credential; no login authorized"})\n            continue\n')
(work/'skills-selected.py').write_text(skills_source)
exec(compile(skills_source,str(work/'skills-selected.py'),'exec'),namespace)
selected_skills=namespace['skill_fixtures']
# Same real fault proxy and oracles; only fixed upstream origin/path change for the authorized gateway.
recovery_source=textwrap.dedent(inspect.getsource(m.Runner.recovery))
old='        proxy = FaultProxy(provider, fault)\n'
assert recovery_source.count(old)==1
recovery_source=recovery_source.replace(old,old+'        if model == WIRE_MODEL: proxy.origin = "openrouter.ai"\n')
old='        url = proxy.start()\n'
assert recovery_source.count(old)==1
recovery_source=recovery_source.replace(old,old+'        if model == WIRE_MODEL: url = url.rstrip("/") + "/api"\n')
(work/'recovery-wire.py').write_text(recovery_source)
exec(compile(recovery_source,str(work/'recovery-wire.py'),'exec'),namespace)
wire_recovery=namespace['recovery']
watch='ec03107a-fcf5-4791-be54-3b8166cd4dd2'
def notify(text):
    subprocess.run(['tuic','mcp','agent',json.dumps({'action':'send','to':watch,'message':text})],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,timeout=15)
class Continuation(m.Runner):
    def allowance(self,provider):
        return min(super().allowance(provider),m.Decimal('0.35')-(self.spent-m.Decimal('0.538596460')))
    def invoke(self,model,workspace,*args,**kwargs):
        start=time.time();mono=time.monotonic()
        notify(f'Continuation invocation starts {model} workspace={workspace}; log={work}/continuation.log')
        result=adapted(self,model,workspace,*args,**kwargs)
        elapsed=time.monotonic()-mono
        with (work/'continuation-invocations.jsonl').open('a') as out:
            out.write(json.dumps({'model':model,'workspace':str(workspace),'start_unix':start,'elapsed_seconds':elapsed,'exit':result['exit'],'reason':result['reason'],'stderr_path':result['stderr_path'],'new_sessions':result['new_sessions']})+'\n')
        notify(f'Continuation invocation ends {model} elapsed={elapsed:.3f}s exit={result["exit"]} stderr={result["stderr_path"]}')
        return result
    def record(self,*args,**kwargs):
        super().record(*args,**kwargs)
        (work/'continuation-cases.json').write_text(json.dumps(self.cases,indent=2)+'\n')
        case=self.cases[-1]
        notify(f'Continuation case {case["model"]} {case["suite"]}/{case["case"]}={case["status"]}; accounted+reserved USD{self.spent}, uncertain={self.uncertain}')
    def cache(self,model):
        # Existing cache successes remain historical; wire cache is scheduled below.
        pass
    def skills(self,model):
        if model in ('openrouter/openai/gpt-6-luna','openrouter/anthropic/claude-haiku-5.5'):
            selected_skills(self,model)
    def compaction(self,model):
        if model==WIRE_MODEL:m.Runner.cache(self,model)
        m.Runner.compaction(self,model)
    def recovery(self,model):
        if model=='openrouter/openai/gpt-6-luna':self.cache_once(model,0)
        elif model==WIRE_MODEL:
            wire_recovery(self,model)
            m.Runner.recovery(self,'openai/gpt-6-luna')
binary=base/'bin/ego-636'
sys.argv=['live-conformance.py','--live','--binary',str(binary),'--work',str(work/'runs'),'--skills',str(base/'fixtures'),'--operator-home',str(home),'--diagnostic-dir',str(work/'continuation-diagnostics'),'--ceiling','0.95','--provider-ceiling','0.90']
# Phase-specific dispatch below avoids repeating completed cells or unrelated wire suites.
args=m.parse_args();args.model=['openai-codex/gpt-6-luna','openai/gpt-6-luna','openrouter/anthropic/claude-haiku-5.5','openrouter/openai/gpt-6-luna',WIRE_MODEL]
# Parse-check the real wire-home facts without contacting providers.
probe=subprocess.run([str(binary),'models','--provider','anthropic','--json'],env=dict(os.environ,EGO_HOME=str(WIRE_HOME),EGO_LOG='off'),capture_output=True,text=True,timeout=30)
(work/'wire-preflight.json').write_text(json.dumps({'exit':probe.returncode,'stderr':probe.stderr,'fact_sha256':hashlib.sha256(haiku.encode()).hexdigest()},indent=2)+'\n')
assert probe.returncode==0,'wire facts rejected before live work'
runner=Continuation(args)
prior_known=m.Decimal('0.236200460');reserve=m.Decimal('0.263268')+m.Decimal('0.039128')
runner.spent=prior_known+reserve
runner.by_provider={'openai-codex':m.Decimal(0),'openai':m.Decimal('0.010302940'),'openrouter':m.Decimal('0.225897520')+reserve}
facts=home/'models.toml';prior=facts.read_bytes() if facts.exists() else None
if prior is not None:(work/'continuation-operator-models-before.toml').write_bytes(prior)
def sha():return hashlib.sha256(binary.read_bytes()).hexdigest()
receipt={'run_id':runner.run_id,'source_commit':subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip(),'binary':str(binary),'binary_sha256_before':sha(),'start_unix':time.time(),'continues':'20261025T183530Z-98100826','binary_source_commit':'05102c930deecf367b448f8540f90339bb46d4f0','prior_unknown_reserves_usd':{'luna':'0.263268','sonnet':'0.039128'},'additional_work_ceiling_usd':'0.35','additional_ceiling_total_upper_usd':'0.888596460','additional_ceiling_router_upper_usd':'0.878293520','prior_known_usd':str(prior_known),'prior_unknown_reserved_usd':str(reserve),'prior_unknown_actual':True,'prior_unknown_reconciled':False,'deadline_override':{'model':'openrouter/openai/gpt-6-luna','cli_seconds':120,'watchdog_seconds':135,'reason':'observed 12.088s startup plus 32.949s first-generation-to-tool leaves insufficient whole-invocation50s budget; not measured TTFT/TPS'},'adapted_invoke_sha256':hashlib.sha256(source.encode()).hexdigest(),'wire_model':WIRE_MODEL,'wire_endpoint':'https://openrouter.ai/api/v1/messages','funding_provider':'openrouter','caps_usd':{'run':'0.95','provider':'0.90'}}
def save():(work/'continuation-receipt.json').write_text(json.dumps(receipt,indent=2)+'\n')
save();rc=2
try:
    facts.write_text(content)
    notify(f'Continuation {runner.run_id} starts; root={runner.work}; receipt={work}/continuation-receipt.json; recovery last, original reserve carried.')
    rc=runner.execute();receipt['runner_exit']=rc
finally:
    if facts.exists() and facts.read_text()==content:
        if prior is None:facts.unlink()
        else:facts.write_bytes(prior)
        receipt['operator_facts_restored']=True
    else:receipt['operator_facts_restored']=False
    receipt.update(binary_sha256_after=sha(),end_unix=time.time(),accounted_plus_reserved_usd=str(runner.spent),new_known_usd=str(runner.spent-prior_known-reserve),new_cost_uncertain=runner.uncertain,by_provider_including_reserve={k:str(v) for k,v in runner.by_provider.items()})
    receipt['stable_endpoints']=receipt['binary_sha256_before']==receipt['binary_sha256_after'];save()
    target=args.results/runner.run_id
    if target.exists():shutil.copyfile(work/'continuation-receipt.json',target/'receipt.json')
    notify(f'Continuation finished rc={rc}; receipt={work}/continuation-receipt.json; target={target}. Please retain diagnoses without further provider calls.')
print(json.dumps(receipt,indent=2),flush=True)
raise SystemExit(rc if receipt['stable_endpoints'] and receipt.get('operator_facts_restored') else 2)
