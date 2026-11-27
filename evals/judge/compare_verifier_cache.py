#!/usr/bin/env python3
"""Compare verified role-specific cache measurements from two Ego journals."""

import argparse
import json
from collections import Counter
from pathlib import Path


def events(path):
    for line in path.read_text().splitlines():
        row = json.loads(line)
        event = row.get('payload', {}).get('event')
        if event is not None:
            yield event['kind']


def collect(path, role):
    starts, usage, decisions, models = set(), {}, [], set()
    authorities = Counter()
    for event in events(path):
        kind = event['kind']
        if kind in ('GenerationStarted', 'UsageUpdated') and event.get('attempt'):
            ident = tuple(event['attempt'][k] for k in ('generation', 'attempt'))
            if kind == 'GenerationStarted' and event['role'] == role:
                starts.add(ident)
                if model := (event.get('cache') or {}).get('model'):
                    models.add(model)
            elif kind == 'UsageUpdated' and ident in starts:
                if ident in usage:
                    raise ValueError(f'duplicate attempt usage: {path}')
                usage[ident] = event['usage']
        if role == 'judge' and kind == 'PermissionDecided':
            source = (event.get('by') or {}).get('by', 'unknown')
            authorities[source] += 1
            if source in ('judge', 'cached_judge'):
                decisions.append({True: 'allow', False: 'deny', None: 'escalate'}[event.get('allowed')])
        elif role == 'goal_check' and kind == 'GoalTransitioned':
            transition = event['transition']
            if transition['kind'] == 'achieved' and transition.get('attested_by') == 'check':
                decisions.append('achieved')
            elif transition['kind'] in ('blocker_observed', 'blocked'):
                decisions.append(transition['kind'])
    if not starts or starts != usage.keys() or len(models) != 1:
        raise ValueError(f'missing or ambiguous {role} usage or model: {path}')
    attempts = [{
        'read': x['cache_read_tokens'],
        'uncached': x['uncached_input_tokens'],
        'written': x['cache_write_tokens'] + x['cache_write_extended_tokens'],
    } for x in usage.values()]
    if any(not sum(x.values()) for x in attempts):
        raise ValueError(f'{role} attempt without positive input: {path}')
    return {
        'model': models.pop(), 'decisions': decisions,
        'provider_calls': len(starts), 'local_cache_avoided_calls': authorities['cached_judge'],
        'permission_authorities': dict(authorities), 'attempts': attempts,
    }


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('role', choices=['goal_check', 'judge'])
    p.add_argument('baseline', type=Path)
    p.add_argument('candidate', type=Path)
    p.add_argument('--expect', nargs='+', required=True, help='Expected durable decisions in chronological order')
    args = p.parse_args()
    before, after = (collect(path, args.role) for path in (args.baseline, args.candidate))
    for arm in (before, after):
        arm['first'] = arm['attempts'][0]
        arm['warm_total'] = {k: sum(x[k] for x in arm['attempts'][1:]) for k in arm['first']}
    valid = (before['model'] == after['model'] and before['decisions'] == after['decisions'] == args.expect
             and before['provider_calls'] == after['provider_calls'] >= 2
             and before['local_cache_avoided_calls'] == after['local_cache_avoided_calls']
             and (args.role != 'judge' or before['local_cache_avoided_calls'] >= 1))
    print(json.dumps({'role': args.role, 'comparable_ledger': valid, 'baseline': before, 'candidate': after}, indent=2))
    return 0 if valid else 1


if __name__ == '__main__':
    raise SystemExit(main())
