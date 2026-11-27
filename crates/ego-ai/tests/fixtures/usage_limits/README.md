# Recorded usage-limit response

`openrouter-key.json` retains only `data.limit`, `data.limit_remaining`,
`data.usage`, and `data.limit_reset` from a real HTTP 200 response to
`GET https://openrouter.ai/api/v1/key` on 2026-10-27 at
14:10:47.363196 UTC. The key was read in memory from the ego repository `.env`.
No key, label, account identity, authorization header, or response body outside
the allowlist was saved. This free endpoint did not make a generation request.
The SHA-256 of the full response was
`f8e77d31840cd7b302811b47430244ac922ba762769daa2044350a073b8e218a`.

The observed remaining value is not `limit - usage`: usage is a distinct
provider counter and the key resets monthly. Tests must retain that distinction.
Null, missing, negative and wrong-type cases are explicit mutations of this
recording, not claims that those responses were observed live.

Header parser cases are adversarial lexical inputs. They are not recorded
Anthropic or OpenAI account responses. No paid call was made to obtain one.
