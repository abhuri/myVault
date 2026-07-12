# v0.1.0-demo Results

วันที่ตรวจ: 2026-07-12 เขตเวลา Asia/Bangkok ค่ะ

## Outcome

Local Desktop Demo ผ่าน automated gates และ live macOS smoke test ค่ะ Frontend กับ save bridge ผ่าน independent deep audit โดยไม่เหลือ P0/P1/P2 ค่ะ

Release: [myVault v0.1.0-demo](https://github.com/abhuri/myVault/releases/tag/v0.1.0-demo) จาก merge commit `0c5fbf0` ค่ะ

## Live macOS Evidence

- native debug `.app` build สำเร็จและเปิดด้วยชื่อ `myVault — Local Demo` ค่ะ
- native folder picker เปิด Synthetic Demo Vault สำเร็จโดย frontend ไม่ส่ง root path argument ค่ะ
- explorer แสดง Markdown 5 ไฟล์และซ่อน `.obsidian` ค่ะ
- อ่าน `Start Here.md` ภาษาไทยพร้อม exact UTF-8 bytes ได้ค่ะ
- Reader แสดง task list, GFM table, highlighted code และ wiki links แบบ inert ได้ค่ะ
- autosave 750 ms เขียนสำเนา Vault ลง disk สำเร็จและ UI แสดง `Saved` ค่ะ
- เมื่อ external writer เปลี่ยนไฟล์ก่อน autosave รอบถัดไป UI แสดง conflict, หยุด autosave, คง editor buffer และไม่เขียนทับ external bytes ค่ะ
- มี `Reload from disk` เป็น explicit recovery action ที่ต้องยืนยันก่อนทิ้ง buffer ค่ะ

## Automated Evidence

- frontend TypeScript ผ่านค่ะ
- Vitest 4 files / 19 tests ผ่านค่ะ
- Vite production build ผ่านค่ะ
- app-service unit 2 + integration 8 tests ผ่านค่ะ
- Tauri Rust tests 6 tests ผ่านค่ะ
- strict Clippy ของ app-service/Tauri ผ่านค่ะ
- Android aarch64 cross-Clippy ผ่านค่ะ
- macOS debug application bundle build ผ่านค่ะ
- `git diff --check` ผ่านค่ะ

## Known Limitations

- Google Drive Sync และ OAuth configuration ไม่อยู่ใน Local Demo ค่ะ
- physical Android acceptance รออุปกรณ์จริงค่ะ
- search/backlinks/graph เป็น prototype จากชื่อไฟล์และโน้ตที่เปิดแล้ว ไม่ใช่ persistent full-vault index ค่ะ
- Reader link navigation ถูกปิดทั้งหมดใน Demo เพื่อป้องกัน WebView top navigation ค่ะ
- Mermaid/graph ทำให้ main JavaScript chunk ประมาณ 1.05 MB minified; code splitting เป็น P3 follow-up ค่ะ
- frontmatter แสดงแบบ Markdown พื้นฐาน ยังไม่มี properties editor เต็มรูปแบบค่ะ

## Release Checksums

```text
c07846b60ed70fd166bd7c2ec451245397da747aec3e29a11ebcfb4c90fefe49  myVault-v0.1.0-demo-macos-ad-hoc.zip
c55fa98359bfa3e8c6ccd702091e57a364c35146d1b99a644b8fbe04be02c125  myVault-v0.1.0-demo-windows-x64-unsigned-setup.exe
2e45d33fc0d81a7aaeeeadda137af85cbc6f60cc946996a14b932765ec45416e  myVault-v0.1.0-demo-linux-amd64.AppImage
```
