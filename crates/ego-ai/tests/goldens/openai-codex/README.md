# Codex inline image fixture

`input-image.json` is the JSON example from OpenAI Codex's
`image_detail_roundtrips_all_wire_values` test, fetched on 2026-10-16:

<https://github.com/openai/codex/blob/6ae8b8aa7c608730d4315d59c5c081897ae02780/codex-rs/protocol/src/models.rs#L2826-L2830>

The source defines `ContentItem::InputImage` with a flattened inline
`image_url` and optional `detail`. This fixture records the upstream example;
it is not a captured paid request. Ego uses the same inline data URL shape
and leaves optional detail unset.

Model input modalities were checked against the OpenAI model pages:

- <https://developers.openai.com/api/docs/models/gpt-6-sol>
- <https://developers.openai.com/api/docs/models/gpt-6.1-sol>

Both pages state `Input Text, Image` and `Image Input only`.
The format list comes from
<https://developers.openai.com/api/docs/guides/images-vision>:
PNG, JPEG, WEBP, and non-animated GIF.
