# Live Codex authentication-failure fixture

`auth-401-live.json` is the exact JSON body recorded from the real Codex endpoint on 2026-11-02. The adjacent metadata file records provenance and the body SHA-256.

The recorder sent `POST https://chatgpt.com/backend-api/codex/responses` with body `{}`, `Content-Type: application/json`, `Accept: text/event-stream`, and `User-Agent: ego-fixture-capture/818`. It sent no Authorization header or account identifier and read no token or credential store. The response was HTTP 401, `application/json`.

The recorder held at most 65537 response bytes in memory. It wrote a fixture only when the status was 401, the content type was JSON, the body was at most 65536 bytes, and the parsed document was exactly `{"detail":"Unauthorized"}`. It recorded no response headers, cookies, request identifiers, credentials, or account identifiers. No body redaction was necessary.

This is a live authentication-failure shape, not a successful authenticated-generation fixture. It does not measure token lifetime or validate subscription terms. The regression replays this body with a synthetic external token through the real provider resolution and generation stack to verify typed refusal and secret-free failure/settlement output.
