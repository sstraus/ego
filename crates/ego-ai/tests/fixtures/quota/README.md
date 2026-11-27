# Quota and rate-limit error fixtures

Story `921-010d`. These are not recordings. No exhausted account was available, and Boss approved on 2026-11-08 deriving the fixtures from current provider documentation. A later real capture replaces a derived case. Each file states its provenance, its source and the date it was read:

- `VendorSchemaDerived`: assembled from the provider's own published error documentation.
- `CapturedClientDerived`: taken from the provider's official client source, not from a server response.

Each case lists the HTTP status, an optional `retry-after` value in seconds, the JSON body and the expected class:

- `refused`: account quota, credit or spend limit. No retry.
- `transient`: rate limit or overload. Retry with capacity backoff.
- `invalid`: the request is wrong.

The message texts use the documents' wording, and the identifiers in them are illustrative.
