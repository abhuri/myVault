# v0.1.0-demo Acceptance

## Purpose

เกณฑ์นี้ใช้ยืนยัน Local Desktop Demo บน macOS ด้วย Synthetic Demo Vault โดยไม่ต้องใช้ Google Drive, hosting หรืออุปกรณ์ Android จริงค่ะ

## Preconditions

- build จาก commit ที่จะ tag เป็น `v0.1.0-demo` ค่ะ
- เปิด `demo/synthetic-vault` ผ่าน native folder picker ค่ะ
- ไม่แก้ fixture ต้นฉบับโดยตรงในการทดสอบซ้ำ ให้ copy ไป temporary folder ก่อนค่ะ

## Required User Journey

1. เปิดแอปโดยยังไม่มี active Vault และเลือก Vault ผ่าน native picker ค่ะ
2. Explorer แสดง Markdown ทั้ง 5 ไฟล์ตามโฟลเดอร์ แต่ไม่แสดง `.obsidian` หรือ `.trash` ค่ะ
3. เปิด `Start Here.md` แล้วเห็นภาษาไทย ตาราง task list code block และ Mermaid ค่ะ
4. แก้ข้อความ รอ 750 ms และยืนยันว่าไฟล์บน disk เปลี่ยนแบบ revision-checked ค่ะ
5. กด `Cmd+S` เพื่อ manual save และเห็นสถานะกลับเป็น clean ค่ะ
6. แก้ไฟล์เดียวกันจากภายนอกก่อน autosave แอปต้องแจ้ง conflict และห้ามเขียนทับค่ะ
7. สลับ Reader mode แล้ว HTML ต้องถูก sanitize; script/event handler ที่ใส่ทดสอบต้องไม่ทำงานค่ะ
8. ใช้ search/quick switcher หา `local-first` และเปิดผลลัพธ์ด้วย keyboard ได้ค่ะ
9. Outline, backlinks และ graph ต้องเชื่อม `Start Here`, `myVault Demo` และ `Local-first Safety` ได้ค่ะ
10. ย่อหน้าต่างเป็น 760 px และตรวจ layout; ตรวจ frontend viewport ที่ 412 px และ 360 px โดยไม่มี horizontal content loss ค่ะ

## Automated Gates

- frontend typecheck, unit tests และ production build ผ่านค่ะ
- Rust fmt, strict Clippy และ tests ของ core/app-service/Tauri ผ่านค่ะ
- Android aarch64 compile/Clippy ผ่านโดยไม่มี desktop dialog dependency ค่ะ
- GitHub quality, Android compile, Windows NSIS และ Ubuntu AppImage checks ผ่านค่ะ
- `git diff --check` และ secret scan ผ่านค่ะ

## Deferred and Non-blocking

- Google OAuth/Drive Sync ถูกเลื่อนไปหลัง Local Demo ค่ะ
- physical Android IME, picker และ lifecycle validation ถูกเลื่อนจนกว่าจะมีอุปกรณ์ค่ะ
- Store distribution, signing และ auto-update ไม่ใช่ acceptance ของ Demo ค่ะ

