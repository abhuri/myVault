# Phase 1 Hardening — Copy-of-Vault Acceptance

## Purpose

This runbook was used on 2026-07-13 to test the then-uncommitted Phase 1 build against a disposable copy of the Synthetic Demo Vaultค่ะ The tested implementation was later captured by `66c299f` and `cbde0c1` ค่ะ The acceptance checks watcher refresh, clean-note reload, dirty-buffer conflict protection, repeated guarded saves, and desktop recovery snapshots without touching a real Vaultค่ะ

## Safety Preconditions

- Close Obsidian and any other editor that points at a real Vault before startingค่ะ
- Use only a newly created copy of `demo/synthetic-vault`; never select the fixture source or a personal Vaultค่ะ
- Record the temporary copy path and confirm it is outside the repository before editingค่ะ
- Do not delete, reset, checkout, clean, stage, or commit the project working tree during this acceptanceค่ะ
- Android SAF writes have a weaker publication contract than desktop writes: they use a synchronized revision check, in-place provider write, descriptor sync, and byte-for-byte readback, but cannot promise desktop-equivalent atomic rename or parent-directory fsyncค่ะ

## Prepare a Disposable Vault

Run these commands from the repository rootค่ะ

```sh
uat_root="$(mktemp -d /tmp/myvault-phase1-uat.XXXXXX)"
ditto demo/synthetic-vault "$uat_root/Vault"
printf '%s\n' "$uat_root/Vault"
```

Keep the printed path for the steps belowค่ะ Do not remove the temporary directory until every expected file and snapshot result has been recordedค่ะ

## Desktop Journey

1. Build and launch the latest macOS application, then choose only the printed temporary `Vault` folderค่ะ
2. Confirm Explorer shows the expected Markdown notes while `.obsidian` and `.trash` remain hiddenค่ะ
3. Open a note and leave its buffer cleanค่ะ Change that copied file from an external editor and confirm the active Reader/Editor content and Explorer refresh without reopening the Vaultค่ะ
4. Edit a note inside myVault, then change the same copied file externally before autosave completesค่ะ Confirm myVault reports a stale-revision conflict, preserves the editor buffer, and leaves the external bytes unchangedค่ะ
5. Use explicit reload only after reviewing the preserved bufferค่ะ Confirm the external content becomes the active clean revisionค่ะ
6. Save three distinct revisions sequentially, waiting for `Saved` after each revisionค่ะ Reopen the copied file after every save and confirm the exact UTF-8 bytes match the latest editor contentค่ะ
7. Confirm recovery snapshot objects were added beneath the app's private `recovery-snapshots/v1/vaults` store and that at least the immediately previous payload is byte-exactค่ะ Do not edit or remove snapshot evidence during acceptanceค่ะ
8. Switch to Reader mode and verify Page Up, Page Down, Space, Shift+Space, Home, End, and Cmd/Ctrl+Arrow navigationค่ะ
9. Open a note containing one invalid Mermaid fence followed by a valid Mermaid fenceค่ะ Confirm the first shows an isolated render error and the later valid diagram still rendersค่ะ
10. Close and reopen the application, select the same disposable Vault, and confirm Explorer and note reads remain coherentค่ะ

## Android Emulator Journey

1. Install the verified debug APK and select a disposable document tree through the system Storage Access Framework pickerค่ะ
2. Confirm the URI grant survives force-stop and cold relaunch without exposing a `content://` URI to the webviewค่ะ
3. Confirm `.obsidian` and `.trash` cannot be inventoried, read, or saved through the native bridgeค่ะ
4. Exercise Thai/Unicode inventory, strict UTF-8 read, stale revision, missing note, invalid path, resource limit, and provider-error mappingsค่ะ
5. Treat `directorySyncUnsupported` as an explicit durability limitation, not as proof of an atomic desktop-equivalent writeค่ะ
6. Keep physical-device OAuth, Thai IME, clipboard, lock/unlock, lifecycle, and real-GPU evidence deferred until a suitable device is availableค่ะ

## Pass Criteria

- No operation touches the source fixture or a personal Vaultค่ะ
- Clean external changes refresh safely, while dirty or uncertain buffers are never auto-replacedค่ะ
- Stale saves never overwrite external bytes and never publish a misleading pre-save snapshotค่ะ
- A configured desktop snapshot failure returns `recoveryUnavailable` and stops before Vault mutationค่ะ
- Repeated desktop saves create byte-exact prior-revision recovery evidenceค่ะ
- Invalid Mermaid input does not block later valid diagramsค่ะ
- Android protected paths and stable error mappings pass automated and emulator checksค่ะ
- All full verification gates listed in the current session handoff pass on the same working treeค่ะ

## Evidence to Record

- Git HEAD and dirty working-tree summaryค่ะ
- macOS application bundle path and build resultค่ะ
- Temporary Vault path, selected note names, and external-change outcomesค่ะ
- Test counts, APK SHA-256, signature/alignment results, and any non-blocking chunk warningค่ะ
- Any regression with exact reproduction steps, while excluding note contents, ambient Vault paths, OAuth credentials, and tokens from logsค่ะ

## Live Run Result — 2026-07-13

Outcome คือ `PASS` สำหรับ macOS local runtime scope ค่ะ รอบนี้ใช้ debug bundle `apps/tauri/src-tauri/target/debug/bundle/macos/myVault.app` กับ disposable Vault `/tmp/myvault-phase1-uat.ykjiuo/Vault` เท่านั้นค่ะ Obsidian ไม่ได้เปิด และไม่มี source fixture, personal Vault หรือ project working-tree file ถูก UAT แก้ไขค่ะ

- Native picker, Explorer filtering, Thai/Unicode read และ clean watcher reload ผ่านค่ะ
- Dirty-buffer external race แสดง stale-revision conflict, รักษา editor buffer และไม่ overwrite external disk revision ค่ะ
- Explicit reload หลังยืนยันโหลด external revision กลับมาและคืนสถานะ `Ready` ค่ะ
- Guarded saves สาม revision จบที่ `937`, `1009`, `1080` bytes ตามลำดับค่ะ
- Recovery payload ก่อนบันทึก `845`, `937`, `1009` bytes เทียบ byte-exact กับ revision ก่อนหน้าได้ทุกก้อนค่ะ
- Reader keyboard navigation ทั้งแปดคำสั่งผ่านค่ะ
- Invalid Mermaid block แสดง isolated error และ valid block ถัดมายัง render ได้ค่ะ
- หลัง close/reopen Explorer แสดงครบ `6` ไฟล์, final note hashes ไม่เปลี่ยน และ snapshot objects `3` ก้อนยังอยู่ครบค่ะ
- Windows/Ubuntu native runtime และ physical Android evidence ไม่ได้ถูกรวมใน PASS นี้และยัง deferred ค่ะ

หลักฐาน SHA-256 และรายละเอียดแต่ละ checkpoint อยู่ใน [Demo Results](RESULTS.md) ค่ะ
