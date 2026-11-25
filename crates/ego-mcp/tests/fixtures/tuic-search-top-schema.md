# TUIC search top-schema fixture

This is a source-derived fixture, not a captured live response.

Source: TUICommander commit `49f31508b1`,
`src-tauri/src/mcp_http/mcp_transport_catalogue.rs`,
`handle_search_tools_for_session`, and the `needle` definition in
`catalogue_schema_reuse_avoids_second_discovery_hop` in the adjacent tests.

The response shape, summary, and complete definition follow that source.
`catalogue_generation` is the SHA-256 hex digest of the compact, sorted-key JSON
array containing the definition, as specified by `ToolSearchIndex::catalogue_generation`.
No runtime backend revision or live discovery-hop measurement is claimed.
