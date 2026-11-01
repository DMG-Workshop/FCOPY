# 0002: Brand and command-name clearance (provisional)

**Status:** Provisional. Binary name chosen (`cmv`). Trademark, distribution, Windows and macOS PATH, domain and GitHub-organisation checks are still open.
**Date of checks:** 2026-11-01
**Context:** Decision 5 in `0001-product-decisions.md` defers the brand name until clearance is complete. This record captures the first pass so the open items stay visible.
**Owner for the final decision:** not yet named.

## Decision in force

- **Product name:** Velox CoreMove. Always use the full name, never `Velox` alone.
- **Binary (CLI) name:** `cmv`. Chosen by the product owner on 2026-11-01, subject to the open checks below.
- **Package name:** `coremove` (crates.io, npm and PyPI all free at time of check).

## Candidates considered

| Binary | Status | Reason |
|---|---|---|
| `cmv` | **Chosen** | Clear of known command-line collisions in the checks below. Crate name `cmv` is taken on crates.io, but a binary name can differ from the crate name. |
| `vcm` | Rejected | Collides with `arden/vcm`, "VCM – Vibe Coding Manager", an MIT-licensed Rust CLI for AI coding tools. Its README describes "One command to manage all your AI coding tools". The installed binary name was not confirmed from its install instructions, but the name strongly suggests `vcm`. |
| `vlx` | Rejected | Taken by an active npm tool, "A very fast JavaScript package manager". |

## Results: package registries

Checked from the build environment with HTTP requests to each registry's public endpoint. A 200 response means the name is taken. A 404 means it appeared free.

| Name | crates.io | npm | PyPI | Command on PATH (Linux) | Verdict |
|---|---|---|---|---|---|
| `velox-coremove` | free | free | free | not applicable | Clear on registries |
| `coremove` | free | free | free | not applicable | Clear on registries |
| `cmv` | taken: unrelated algorithm library | free | taken: placeholder SDK | none found | **Chosen binary name.** Crate-name collision does not affect the binary. |
| `vcm` | free | taken: empty 0.0.0 placeholder | free | none found | Rejected: see above |
| `vlx` | taken: placeholder | taken: active JavaScript package manager | free | none found | Rejected |
| `velox` | taken: web and Rust app framework | taken: "veloxstack.com core module" | taken: ML tooling | not applicable | Avoid as a standalone name |

## Results: other channels

| Check | Result |
|---|---|
| Homebrew core formulas | None exist for `cmv`, `vcm`, `vlx`, `velox`, `velox-coremove` or `coremove`. Third-party taps not checked. |
| GitHub repository search, `cmv` in name | Many unrelated repositories (map viewers, game tools, research code). The `cmv` organisation is taken by a map-viewer project, which affects only a GitHub organisation name, not the binary. |
| GitHub repository search, CLI named `cmv` | No command-line tool called `cmv` found. |
| Web search, "CoreMove" software | No software product named CoreMove found. Results were unrelated products. |
| Web search, "Velox" file-transfer and copy tools | No product named "Velox" in this space found. Results were MOVEit, TeraCopy and other tools. |
| Docker Hub `library/vcm` | Not present. |

## Not yet checked

| Check | Status | Blocker |
|---|---|---|
| USPTO trademark search (`VELOX`, `VELOX COREMOVE`, `COREMOVE`) | Not checked | USPTO endpoint unreachable from build environment |
| WIPO Global Brand Database | Not checked | Endpoint unreachable |
| EUIPO eSearch | Not checked | Endpoint unreachable |
| UK IPO | Not checked | Endpoint unreachable |
| Target-market national registers | Not checked | Not yet identified |
| Linux distributions (Arch, Debian, Fedora, apt, dnf) | Not checked | Package-site hosts refused by the build environment's egress proxy |
| Windows package managers (Winget, Chocolatey, Scoop) | Not checked | Chocolatey endpoint unreachable. Winget path check inconclusive. |
| Container registries beyond Docker Hub `library` (GHCR, other namespaces) | Not checked | Not attempted |
| Command-name collision on Windows and macOS (`cmv` on PATH) | Not checked | Only Linux PATH checked, and only in this environment |
| Domains: `coremove.*`, `velox-coremove.*` | Partly checked: `velox-coremove.com` reported available, about $15 a year. `vcm.dev` reported available, about $115.70 a year (premium). Reported by the product owner from a registrar lookup on 2026-11-01. Not verified from the build environment, which cannot reach RDAP. | `coremove.*` and `cmv.*` not yet checked. Registrar prices and availability can change, so re-check before buying. |
| GitHub organisation `velox-coremove` | Not checked | GitHub user and org API returns 403 for these lookups |
| Social handles | Not checked | Not attempted |
| Common-law use: app stores, Product Hunt, further web search | Partly checked | Four US-only web searches run. App stores not searched. |
| Meaning and pronunciation of `cmv` in target languages | Not checked | Not attempted |
| Acronym collision for `CMV` (for example Change My View, cardiac and other uses) | Not checked | Not attempted |

## Conditions before this becomes final

1. Trademark search completed in USPTO, WIPO, EUIPO and UK IPO for `VELOX COREMOVE` and `COREMOVE`, with results recorded.
2. Counsel reviews the trademark results in the relevant classes, and decides whether the combined name is distinct enough.
3. Linux distributions, Windows package managers and container registries checked for a package or binary named `cmv`.
4. `cmv` command collision checked on Windows and macOS.
5. Domains and the GitHub organisation secured or confirmed available. Registrar reports on 2026-11-01: `velox-coremove.com` available (about $15 a year); `vcm.dev` available (about $115.70 a year). `vcm.dev` no longer matters, since `vcm` is rejected. Still to check: `cmv.dev`, `coremove.com`, and the GitHub organisation `velox-coremove`.
6. App stores searched for "CoreMove" and "Velox CoreMove".
7. Decision 5 in `0001-product-decisions.md` updated to reference this record.

## Re-run commands for the registry checks

```bash
for n in cmv coremove velox-coremove vcm vlx; do
  echo "== $n"
  echo -n "crates.io: "; curl -s -m 15 -A "clearance-check" -o /dev/null -w "%{http_code}\n" "https://crates.io/api/v1/crates/$n"
  echo -n "npm: ";        curl -s -m 15 -o /dev/null -w "%{http_code}\n" "https://registry.npmjs.org/$n"
  echo -n "PyPI: ";       curl -s -m 15 -o /dev/null -w "%{http_code}\n" "https://pypi.org/pypi/$n/json"
  echo -n "Homebrew core: "; curl -s -m 15 -o /dev/null -w "%{http_code}\n" "https://raw.githubusercontent.com/Homebrew/homebrew-core/master/Formula/${n:0:1}/$n.rb"
done
command -v cmv || echo "cmv not on PATH"
```

A 200 response means the name is taken. A 404 means it appeared free at the time of the check. A Homebrew core 404 means no formula with that name is in homebrew-core.
