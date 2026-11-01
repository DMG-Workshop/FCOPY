# 0002: Brand and command-name clearance (provisional)

**Status:** Provisional. Registry checks done. Trademark, Homebrew, distribution, domain, and common-law checks are still open.
**Date of checks:** 2026-11-01
**Context:** Decision 5 in `0001-product-decisions.md` defers the brand name until clearance is complete. This record captures the first pass so the open items stay visible.
**Owner for the final decision:** not yet named.

## Candidates

- **Product name:** Velox CoreMove
- **Binary name, option A:** `vcm`
- **Binary name, option B:** `vlx`
- **Fallback product name:** CoreMove (binary `cmv`), with `coremove` as the package name

## Results: package registries

Checked from the build environment with HTTP requests to each registry's public endpoint. A 200 response means the name is taken. A 404 means it appeared free.

| Name | crates.io | npm | PyPI | Command on PATH (Linux) | Verdict |
|---|---|---|---|---|---|
| `velox-coremove` | free | free | free | not applicable | Clear on registries |
| `coremove` | free | free | free | not applicable | Clear on registries |
| `vcm` | free | taken: empty 0.0.0 placeholder, no description | free | none found | Usable as binary name. npm placeholder needs confirming. |
| `vlx` | taken: placeholder | taken: "A very fast JavaScript package manager" (active, different tool) | free | none found | **Ruled out** |
| `velox` | taken: web and Rust app framework | taken: "veloxstack.com core module" | taken: ML tooling | not applicable | Avoid as a standalone name |
| `cmv` | taken: unrelated algorithm library | free | taken: placeholder SDK | none found | Weak |

Notes:
- The `vcm` npm entry has no description and version 0.0.0. It may be a reservation by someone else. Confirm with npm before relying on the name there. It does not affect a Rust binary called `vcm`, but it would matter if we publish to npm.
- The `velox` name is used by at least three active projects. The product should always be named in full.

## Not yet checked

| Check | Status | Blocker |
|---|---|---|
| USPTO trademark search (`VELOX`, `VELOX COREMOVE`, `COREMOVE`) | Not checked | Database unreachable from build environment |
| WIPO Global Brand Database | Not checked | Unreachable |
| EUIPO eSearch | Not checked | Unreachable |
| UK IPO | Not checked | Unreachable |
| Target-market national registers | Not checked | Not yet identified |
| Homebrew formula names | Not checked | Unreachable from build environment |
| Linux distributions (apt, dnf, AUR) | Not checked | Package index not available in build environment |
| Windows package managers (Winget, Chocolatey, Scoop) | Not checked | Not attempted |
| Container registries (Docker Hub, GHCR) | Not checked | Not attempted |
| Command-name collision on Windows and macOS | Not checked | Only Linux PATH checked |
| Domains: `velox-coremove.*`, `vcm.dev`, `vlx.dev` | Not checked | Not attempted |
| GitHub organisation name | Not checked | Not attempted |
| Social handles | Not checked | Not attempted |
| Common-law use: web, app stores, GitHub, Product Hunt | Not checked | Not attempted |
| Meaning and pronunciation in target languages | Not checked | Not attempted |
| Acronym collision for `VCM` (voice coil motor, video coding, others) | Not checked | Not attempted |

## Provisional recommendation

- Use **Velox CoreMove** as the full product name. Always use the full name, never `Velox` alone.
- Use **`vcm`** as the binary name, pending the trademark search, the npm placeholder check, and the distribution checks.
- Do not use **`vlx`**. It is taken by an active npm tool.
- Keep **CoreMove** (`coremove`) as the fallback product name, since it is clear on the three registries.

## Conditions before this becomes final

1. Trademark search completed in USPTO, WIPO, EUIPO and UK IPO, with the results recorded.
2. Counsel reviews the trademark results for `VELOX COREMOVE` in the relevant classes.
3. Homebrew, Linux distributions, Windows package managers and container registries checked.
4. Command-name collision checked on Windows and macOS.
5. Domains and GitHub organisation secured or confirmed available.
6. Common-law use searched.
7. npm `vcm` placeholder status confirmed.
8. Decision 5 in `0001-product-decisions.md` updated to reference this record.

## Re-run commands for the registry checks

```bash
for n in vcm vlx velox-coremove velox cmv coremove; do
  echo "== $n"
  echo -n "crates.io: "; curl -s -m 15 -A "clearance-check" -o /dev/null -w "%{http_code}\n" "https://crates.io/api/v1/crates/$n"
  echo -n "npm: ";        curl -s -m 15 -o /dev/null -w "%{http_code}\n" "https://registry.npmjs.org/$n"
  echo -n "PyPI: ";       curl -s -m 15 -o /dev/null -w "%{http_code}\n" "https://pypi.org/pypi/$n/json"
done
```

A 200 response means the name is taken. A 404 means it appeared free at the time of the check.
