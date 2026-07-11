# myVault

`myVault` is a local-first, cross-platform Markdown vault application with direct Google Drive synchronizationค่ะ

The first release targets macOS, Windows, Ubuntu, and Android through Tauri 2 with a React and TypeScript UI and a Rust native coreค่ะ

## Status

The project is in Phase 0, which is focused on validating platform support, local storage, OAuth, Google Drive access, and the editor stack before full product developmentค่ะ

See [PROJECT_PLAN.md](PROJECT_PLAN.md) for architecture decisions, delivery phases, safety rules, and the current session handoffค่ะ

## First-release constraints

- Personal use onlyค่ะ
- No application backendค่ะ
- No hosting or VPNค่ะ
- No App Store or Play Store distributionค่ะ
- No real vault data, OAuth tokens, signing keys, or credentials may be committedค่ะ

## Development

Install dependencies and run the Phase 0 native shellค่ะ

```bash
pnpm install --frozen-lockfile
pnpm dev
```

Run the baseline verification contractค่ะ

```bash
pnpm typecheck
pnpm test
pnpm build
cargo fmt --manifest-path apps/tauri/src-tauri/Cargo.toml --all -- --check
cargo clippy --manifest-path apps/tauri/src-tauri/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --manifest-path apps/tauri/src-tauri/Cargo.toml
pnpm tauri:info
```

Phase 0 platform gates and environment gaps are documented in [docs/phase-0](docs/phase-0)ค่ะ
