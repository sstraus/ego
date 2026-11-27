# εgo website

Static landing site. The `website` workflow (`.github/workflows/website.yml`)
publishes it at the root of ego.tuicommander.com and the mdBook user guide (`docs/`) under
`/docs/`. There is no Node toolchain, bundler or framework: the files here are
the site.

| File | Role |
| --- | --- |
| `index.html` | Landing page: hero permit sequence, then five evidence-backed strengths (control, token economy, sessions, scripts and CI, hosts and providers) and details |
| `install.html` | Homebrew, verified download, source build, first run |
| `style.css` | All styles; light and dark through `prefers-color-scheme` |
| `main.js` | Copy buttons and one small engine that plays each demo once when it scrolls into view, with Replay; the site works without it |
| `favicon.svg` | ε mark |
| `fonts/` | JetBrains Mono (SIL Open Font License 1.1), self-hosted so the site makes no third-party requests |

## Design rules

- Light, airy surfaces; one cobalt accent for an action that may run. Green,
  amber and red mean granted, asks and refused, nothing else.
- Motion must show a real εgo behaviour. One orchestrated hero moment (five
  proposed actions crossing the permit line), then one small demo per strength
  that plays once on view. No decorative particles, glows or tilts.
- Positioning comes from `docs/internals/point-zero/08-strengths.md` (private).
  Do not claim binary size, startup speed, memory figures, Windows
  confinement, A2A beyond discovery, or per-host network rules for shell commands.
- Every demo resolves to its final state under
  `prefers-reduced-motion`. The page stays readable without JavaScript.
- Every claim and command must match the user guide and the CLI. A partial
  capability is described as partial (A2A, Windows confinement). Example output
  must not invent event schemas.
- No analytics, no external fonts, no third-party scripts.

## Preview locally

CI has no private `docs/internals/` (it is untracked). In a working checkout
that tree can break `mdbook build`, so build the guide from a copy without it:

```sh
dest=~/Gits/.tmp/ego-site book=~/Gits/.tmp/ego-book
mkdir -p "$dest" "$book/tools" && cp -R website/. "$dest/"
rsync -a --exclude internals --exclude book docs "$book/" && cp -R tools/docs "$book/tools/"
mdbook build "$book/docs" --dest-dir "$dest/docs"
python3 -m http.server 8765 --directory "$dest"
```

## Before the first deploy

Published at <https://ego.tuicommander.com>; the guide at `/docs/`.
`website.yml` is the only Pages workflow: remove `.github/workflows/docs.yml`
from the public tree in the release that introduces it.

1. DNS: done — `CNAME` `ego` → `sstraus.github.io` (DNS only) in the `tuicommander.com` zone.
2. Pages: done (source "GitHub Actions", custom domain `ego.tuicommander.com`).
   Enforce HTTPS once the certificate is issued. Actions deployments ignore a `CNAME` file.
3. Add a 1200×630 `og.png` and reference it from `og:image`.
