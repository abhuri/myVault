# myVault Tauri Demo

Native shell ใช้ Tauri 2, React, TypeScript และ Rust ค่ะ UI direction หลักบันทึกไว้ใน [DESIGN.md](DESIGN.md) ค่ะ

## Run

จาก repository root ค่ะ

```bash
pnpm install --frozen-lockfile
pnpm --dir apps/tauri tauri dev
```

เมื่อแอปเปิด ให้กด `Choose Vault folder` และเลือก copy ของ `demo/synthetic-vault` ค่ะ

## Verify

```bash
pnpm --dir apps/tauri typecheck
pnpm --dir apps/tauri test
pnpm --dir apps/tauri build
cargo clippy --manifest-path apps/tauri/src-tauri/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --manifest-path apps/tauri/src-tauri/Cargo.toml
```

Demo เป็น local-only ค่ะ Google Drive, Android arbitrary folder picker และ public distribution ถูกเลื่อนไว้ค่ะ
