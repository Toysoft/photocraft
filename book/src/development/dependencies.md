# Dependencies

Cargo dependencies are pinned through `Cargo.lock` and exercised by the normal workspace build, test, clippy, and WebAssembly jobs. Parser and GPU dependencies deserve additional review because they process attacker-controlled data or interface with native drivers.

## Current repository state

No `cargo-deny` policy file, `cargo-audit` job, or equivalent dependency-advisory workflow is present in the current tree. Do not describe dependency auditing as implemented until configuration and CI evidence land. Update automation is configured in `.github/renovate.json5`, but it only takes effect once the Mend Renovate App is installed for the repository.

## Proposed checks

```sh
cargo audit
cargo deny check
```

A future `deny.toml` should be reviewed by maintainers and encode accepted licenses, advisory policy, source rules, and duplicate-version policy. Advisory exceptions need an owner, rationale, compensating control, and expiration/review date. CI tools and GitHub Actions should also be version-pinned and reviewed as dependencies.

Dependency changes should document why a crate is needed, whether it processes untrusted input, unsafe/native code exposure, supported targets, maintenance status, and license compatibility. Avoid replacing source review with a scanner result.

## Update automation (Renovate)

Renovate keeps Cargo crates and GitHub Actions current. The policy lives in `.github/renovate.json5` and runs through the Mend-hosted Renovate App (installing the app is a maintainer step; until then nothing runs).

- **Schedule**: a weekly batch, Monday before 06:00 UTC.
- **Cooldown**: a release must be 7 days old before Renovate proposes it (`minimumReleaseAge`), so a compromised or broken release is normally caught first. Two cases cannot enforce it: the pinned `craft-libs` git revisions have no release timestamp, and lockfile maintenance resolves through Cargo itself rather than as per-package updates.
- **Grouping**: Cargo patch updates form one pull request and minor updates another, GitHub Actions another; majors are separate.
- **Merging**: patch and digest updates auto-merge, but only once the repository has required status checks (branch protection); without them the platform can merge before CI starts or after it fails. Major updates need approval on the Dependency Dashboard.
- **Actions**: pinned to commit digests (`helpers:pinGitHubActionDigests`); the existing Action tags stay mutable until the first pin pull request merges.
- **Security**: GitHub security advisories raise a pull request immediately, bypassing the schedule and the cooldown. `osvVulnerabilityAlerts` is experimental, covers direct dependencies only, and fires only once a fix exists; it needs the dependency graph and Dependabot alerts enabled. It does not replace manual work on unmaintained crates (issue #4983).

To change the policy, edit `.github/renovate.json5` and validate it with `npx --package renovate renovate-config-validator`, then update this section if the choice changed. The `dependencies` and `security` labels are applied to Renovate pull requests; create them if missing.
