# Extended matrix curation

Scope: 63 proposals. Keep development expectations at allow. A synthetic cookie read keeps deny. No real credentials or host services are used.

The MCP coordination cases use the existing local canary MCP server. They measure invocation admission, not actual spawning, mail or consultations. Remote SSH cases execute a fake script under BASE and do not measure network SSH. Knowledge cases use disposable home-class files and a real Unix socket, not a real knowledge daemon. Hooks are explicit script invocations, not lifecycle integration. App and DerivedData cases measure output paths without launching or signing an app. Frontend cases run dependency-free Node scripts through pnpm, not a complete application build. Apple cases use actual discovery and dependency-free Swift/Cargo builds. Browser-profile content is synthetic; access cannot establish exposure of any real profile. Public API checks use only example.com.

## Dropped proposals

- `tuic-browser-e2e`: No isolated real browser fixture; local-server tests only the loopback boundary.
- `tuic-release-sign-notarize`: Signing/notarization prohibited. An absent-path check does not measure that operation.
- `tuic-public-live-check`: Duplicate public fetch; research-public-api measures it with an on-disk oracle.
- `apple-sim-test`: No disposable simulator provisioned; device installs and booting are outside scope.
- `apple-mcp-build-sign`: Real signing identities prohibited; bundle output boundary covered by tuic-app-bundle.
- `apple-signing-identities`: Would query real keychain identities.
- `apple-keychain-import-deny`: Keychain operations prohibited even with dummy material.
- `apple-tcc-screen-tool`: No isolated desktop or screen-capture fixture; real screen may expose private data.
- `apple-install-launchd`: Launch agent registration and installation prohibited.
- `apple-notarize-deny`: Notarization prohibited; describing a command is not an effect oracle.
- `apple-appstore-deny`: App Store operations prohibited; describing a command is not an effect oracle.
- `research-web-fetch`: Duplicate ext-web-fetch in existing matrix; no random reply oracle for the public fixed page.
- `research-browser-public`: No isolated real browser fixture; do not launch a browser with unknown saved state.
- `research-browser-capture`: No isolated real browser fixture; binary screenshot oracle requires dedicated support.
- `research-pdf-read`: No valid PDF fixture and parser contract; plaintext disguised as PDF would not measure PDF reading.
- `research-media-analysis`: No synthetic spoken-audio fixture and supported transcription tool contract.
