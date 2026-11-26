# Prompt-cache probe

Live check that a model reuses the provider prompt cache as ego encodes it.
Information only: no gate. It spends real tokens (about 5 requests of ~15k input
tokens per session).

## What it measures

`probe.py` writes a synthetic workspace (a ~15k-token system text and four marker
files), then runs `--sessions` fresh `ego run` sessions in sequence. Each session
is one turn with five requests that share the system prefix.

- **Read share per request**: cache read / total input.
- **First request**: read share of request 1 of each new session. It shows whether
  a new session can reuse the prefix that the previous session cached.
- **Warm lag**: for consecutive requests in one turn, previous total input minus
  (cache read + cache write) of the next request. A healthy encoder has a lag
  near zero: the next request reads or writes the whole previous request.
- **Oracle**: the system text requires `AUTHORITY-ORACLE-5521` as the last line.
  `MISSING` means the model did not honor the system text, so a cache change
  that moves the system text also changed its authority.

## Run

```sh
python3 -I evals/cache/probe.py --model <provider/model> [--account N] [--sessions 5] [--bin path/to/ego]
python3 -I evals/cache/probe.py --analyze <out-dir>
python3 -I evals/cache/probe.py --survey
```

Expected for a healthy model: warm lag near-full for nearly every pair, and the
oracle `ok` in every session. A warm lag of about the system size on every pair
means part of the prefix is never read from cache (see the 6.2-sol record below).
Isolated zero reads on early requests of young sessions were observed on
gpt-6.2-sol (provider routing is suspected, not proven); repeat before acting on
one run.

`--survey` computes the warm lag per model over every local session journal and
spends no tokens. Run it first: a model with a near-full share far below 95% is
a candidate for a live probe.

## When to run

- After a change to a wire encoder in `crates/ego-ai/src/wire/` or to
  `crates/ego-ai/src/wire/cache.rs`.
- When a provider adds a model family, before it becomes a default.
- For every provider not yet in the table below.

## Records

| Date | ego | Model | Sessions | First request median read | Warm near-full | Oracle | Note |
|---|---|---|---|---|---|---|---|
| 2026-11-20 | 0.4.17 | openai-codex/gpt-6.2-sol | 1 | 0% | 0/4 (lag ~20k) | ok | system text in `instructions` is never read from cache |
| 2026-11-20 | 0.4.18 | openai-codex/gpt-6.2-sol | 1 | 0% | 3/4 | ok | system text as the leading developer item |
| 2026-11-20 | 0.4.18 | openai-codex/gpt-6.2-sol | 5 | 0% (one session 98.5%) | 16/17 | ok | one isolated zero read at request 3 |
| 2026-11-20 | 0.4.18 + 1177-f2d8 | openai-codex/gpt-6.2-sol | 6 | 98.8% (4 of 5 later sessions; session 1 cold) | 24/24 | ok | shared prefix key in body and headers |
| 2026-11-20 | 0.4.18 | xai-oauth/grok-4.7 | 5 | 4.2% (later sessions: 4.2, 64.3, 99.0, 4.2%) | 20/20 | ok | per-session `prompt_cache_key` |
| 2026-11-20 | 0.4.18 | xai-oauth/grok-4.7 | 5 | 64.1% (later sessions: 4.2, 64.1, 99.2, 99.2%) | 20/20 | ok | per-session key, repeat (salt `key2`) |
| 2026-11-20 | 0.4.18 | xai-oauth/grok-4.7 | 5 | 99.4% (later sessions: 64.3, 99.4, 99.5, 99.4%) | 20/20 | ok | key omitted (`-c cache="none"`, salt `omit1`) |
| 2026-11-20 | 0.4.18 | xai-oauth/grok-4.7 | 5 | 64.4% (later sessions: 4.2, 64.4, 99.2, 99.2%) | 20/20 | ok | key omitted, repeat (salt `omit2`) |
| 2026-11-20 | 0.4.18 | anthropic/claude-sonnet-5-5 | 5 | 99.2% (4 of 4 later sessions; session 1 writes) | 20/20 | ok | through the user protocol filter |
| 2026-11-20 | 0.4.18 | gemini/gemini-3.8-flash | 5 | n/a | n/a | n/a | every session failed: provider "high demand"; no usage |
| 2026-11-20 | 0.4.18 + 1177-f2d8 | openai-codex/gpt-6-luna | 5 | 98.4% (4 of 4 later sessions; session 1 cold) | 20/20 | ok | full-size prefix: the survey share of 83% came from small prefixes |
| 2026-11-20 | 0.4.18 + 1177-f2d8 | gemini/gemini-3.8-flash | 5 | 0% | 0/1 | MISSING | free-tier key: HTTP 429 after 20 requests per day, one timeout; the one warm pair read nothing (see story) |
| 2026-11-23 | 0.4.18 | gemini/gemini-3.8-flash | 0 probe sessions | n/a | n/a | n/a | free-tier preflight: HTTP 503, then HTTP 429; stopped after two requests with retries disabled (#1178-599d) |

The five-session Gemini rerun requires a paid-tier credential with enough quota
for 25 requests at the default probe size. The configured free-tier project was
reported to allow 20 requests per day. The reduced plan of five sessions with
three requests each would have used 17 requests including preflight. The
preflight 429 stopped the run before the first probe session.

Google documents implicit caching for stateless Interactions requests and a
4,096-token minimum for Gemini 3.8 Flash. The model's Standard pricing lists
free-tier context caching. Hits remain opportunistic. The November 20 journal
recorded 34,234 and 34,394 input tokens for the sole completed warm pair, with
zero recorded cache reads. It retained a system snapshot and matching tool
catalogue digests, but lacks the serialized HTTP request bodies. Attribution of
that miss remains open. See [Google's caching guide](https://aistudio.google.com/docs/caching#implicit-caching)
and [model pricing](https://aistudio.google.com/docs/pricing#gemini-3.8-flash).

Cross-check report for these rows: `docs/internals/2026-11-20-cache-crosscheck.md`.

Affinity-key comparison, 2026-11-20, experimental build of 0.4.18, 6 fresh sessions
per arm, distinct prefix per arm. "Shared" key = digest of model, leading
developer item and tools. Sessions 2–6 of an arm can reuse the prefix of session 1.

| Arm | Body `prompt_cache_key` | `session-id` / `x-client-request-id` | 6.2-sol first request (sessions 2–6 at ~99%) | 6.1-sol first request (sessions 2–6 at ~99%) |
|---|---|---|---|---|
| session (current) | per session | per session | 1/5 | 1/5 |
| shared | shared | per session | 0/5 | 0/5 |
| shared-all | shared | shared | 5/5 | 5/5 |
| omit | absent | per session | 0/5 | 1/5 |

The `session-id` header, not the body key, decides cross-session reuse. Warm
requests stay near-full in every arm (21/21 to 24/24 per arm, one isolated zero
read in most arms). The authority line was missed by 6.1-sol on both 0.4.17 (5/6)
and 0.4.18 (6/6) because the fixture then also said "no other text"; the fixture
now asks for the line explicitly.

Concurrency, same day, shared-all arm on gpt-6.2-sol: one warm-up session, then
four concurrent sessions with the same header. All four read 98.6% on request 1
and returned the correct output: a shared `session-id` does not mix sessions.
Story 1177-f2d8 makes the shared key the Codex default.

Survey 2026-11-20 (`--survey`, journals up to ego 0.4.17): near-full share 97–100%
for gpt-6-sol, 6.1-sol, 6.1-terra, 6-astra, claude-opus-5-5 and claude-sonnet-5-5;
83% for gpt-6-luna (small prefixes); 0% for gpt-6.2-sol before 0.4.18.

Open: `openai/*` (API key) and Bedrock routes have no credential on this machine;
Gemini needs a paid-tier key for a full 5-session probe.
