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

## Phase 1 Hardening — Live Copy-of-Vault UAT — 2026-07-13

### Outcome

Live macOS scope ผ่านตาม [Phase 1 Hardening — Copy-of-Vault Acceptance](PHASE1_HARDENING_ACCEPTANCE.md) ค่ะ การทดสอบใช้เฉพาะ disposable Vault `/tmp/myvault-phase1-uat.ykjiuo/Vault` และ debug application bundle `apps/tauri/src-tauri/target/debug/bundle/macos/myVault.app` ค่ะ Source fixture, personal Vault และ project working tree ไม่ถูกแก้ไขโดย UAT ค่ะ

ผลนี้ยืนยัน macOS local runtime ของ current uncommitted Phase 1 build เท่านั้นค่ะ ไม่ใช้แทน Windows/Ubuntu native runtime หรือ physical Android evidence ค่ะ

### Watcher and Conflict Evidence

- Native picker เปิด Vault สำเนาและ Explorer แสดง Markdown เริ่มต้น `5` ไฟล์โดยซ่อน `.obsidian` ค่ะ
- Clean external change รีโหลดโน้ตที่เปิดอยู่จาก `680` เป็น `758` bytes โดยอัตโนมัติและไม่แสดง conflict ค่ะ
- Dirty-buffer race รักษา internal editor buffer `862` bytes ไว้ แสดง stale-revision conflict และหยุด autosave ค่ะ
- External disk revision คงอยู่ที่ `845` bytes, SHA-256 `afd11a8261b9658c079a5f0fef13bf9be61a41a1125facefa90c0337e2b569d2` และไม่มี internal marker ถูกเขียนทับลง disk ค่ะ
- หลังยืนยัน explicit `Reload from disk` editor โหลด external revision `845` bytes กลับมาและ conflict หายค่ะ

### Guarded Saves and Recovery Evidence

- Sequential revision 1 บันทึกเป็น `937` bytes, SHA-256 `9c309c867bd77ba2e12322dc7ad82d00176e2744d62918de9cd824433cf22db1` ค่ะ
- Sequential revision 2 บันทึกเป็น `1009` bytes, SHA-256 `80c86cb607c10aaa078d75150a69fd93ab2aed7529f1e533ba44cf17e7ca7dda` ค่ะ
- Sequential revision 3 บันทึกเป็น `1080` bytes, SHA-256 `b2d25fa7bca62cd8388c543408bf5c108999a181b7d412bb4b60bb0e4f357dbb` ค่ะ
- Recovery snapshot objects ถูก publish `3` ก้อนพร้อม `reason=before_content_replace` และ path `Notes/ภาษาไทยและ Unicode.md` ค่ะ
- Payload ก่อนบันทึกมีขนาด `845`, `937`, `1009` bytes และเทียบกับ revision ก่อนหน้าได้ `byteExact: true` ทุกก้อนค่ะ
- Snapshot payload SHA-256 ตามลำดับคือ `afd11a8261b9658c079a5f0fef13bf9be61a41a1125facefa90c0337e2b569d2`, `9c309c867bd77ba2e12322dc7ad82d00176e2744d62918de9cd824433cf22db1` และ `80c86cb607c10aaa078d75150a69fd93ab2aed7529f1e533ba44cf17e7ca7dda` ค่ะ

### Reader, Mermaid, and Restart Evidence

- Watcher เพิ่ม `Notes/UAT Reader Mermaid.md` เข้า Explorer โดยอัตโนมัติ ทำให้จำนวนไฟล์เปลี่ยนจาก `5` เป็น `6` ค่ะ
- UAT Reader note มีขนาด `5222` bytes และ SHA-256 `9629823810f0964451b4c1daf1842ad3c23e28112c31390681d88c46e8ba7c50` ค่ะ
- `Page Down`, `Page Up`, `Space`, `Shift+Space`, `Home`, `End`, `Cmd+Down` และ `Cmd+Up` เลื่อนภายใน Reader ได้ถูกต้องค่ะ
- Invalid Mermaid fence แสดง isolated render error ส่วน valid Mermaid fence ที่ตามมายัง render diagram ได้ค่ะ
- หลังปิดและเปิด debug build ใหม่ Explorer แสดงครบ `6` ไฟล์, sequential revision ทั้งสามยังอยู่, Reader/Mermaid ให้ผลเดิม และ snapshot objects `3` ก้อนยังมี payload hash เดิมค่ะ
- ไม่พบ regression ใน live macOS journey รอบนี้ค่ะ

### Deferred Evidence

- Windows/Ubuntu native picker persistence, Trash/Restore และ secret-store restart evidence ยัง deferred จนกว่าจะมี native environment ค่ะ
- Physical Android Play Services consent, Thai IME, clipboard, lock/unlock, lifecycle และ real-GPU evidence ยัง deferred จนกว่าจะมีอุปกรณ์จริงค่ะ
