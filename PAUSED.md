# myVault — Paused Project Checkpoint

Project status: **PAUSED** on 2026-07-18 Asia/Bangkok ค่ะ

โครงการถูกพักโดยเจตนา ไม่ใช่ Project Complete และไม่ใช่การยืนยันว่า release
พร้อมใช้งานค่ะ Source ถูกเก็บเพื่อให้ตรวจสอบย้อนหลังหรือกลับมาพัฒนาต่อได้โดยไม่ต้อง
เริ่มใหม่ค่ะ

## เหตุผลที่พักโครงการ

เป้าหมายส่วนตัวหลักคือแอป Markdown แบบ Obsidian ที่ใช้ Google Drive ของผู้ใช้และ
Sync ข้ามอุปกรณ์ได้ค่ะ การตรวจตลาดพบว่า Obsidian ร่วมกับ Community Sync plugin
สามารถตอบโจทย์ส่วนตัวได้ดีพอโดยมีต้นทุนต่ำหรือไม่มีค่าบริการบังคับในปัจจุบันค่ะ
จึงไม่คุ้มใช้ engineering usage ต่อเพื่อสร้างความสามารถที่มีทางเลือกพร้อมใช้อยู่แล้ว
จนกว่าการทดลองจริงจะพบช่องว่างสำคัญค่ะ

## Source of Truth ตอนพัก

- Canonical stable branch คือ `main` ค่ะ
- Incomplete R3.5 prerequisite candidate อยู่ที่ commit `4f0ba27711ea26f0a38b7dcfcc7d94ae1f439b40`
  และ Draft PR #30 ค่ะ Candidate นี้ไม่ถูก merge เข้า `main` ค่ะ
- งาน R3.4 proof-only Android SAF capability ถูกเก็บบน branch
  `codex/r3-4-completion` ค่ะ Shipped allowlist ยังคงว่างและไม่มี production
  mutation adapter ค่ะ
- Annotated archive tag คือ `paused-2026-07-18-r3-4` ค่ะ
- สถานะผลิตภัณฑ์โดยประมาณยังเป็น 40–45% ของ Personal First Release เมื่อวัดจาก
  user-visible outcome ค่ะ

รายละเอียด implementation, verification, known gaps และ safety contracts อยู่ใน
[SESSION_HANDOFF.md](SESSION_HANDOFF.md), [PROJECT_PLAN.md](PROJECT_PLAN.md) และ
เอกสารใต้ `docs/sync/` ค่ะ

## Verification ตอนพัก

- `pnpm quality:r2:offline` ผ่านบน Source checkpoint ที่จะ Archive ค่ะ ชุดนี้รวม
  TypeScript typecheck, Frontend 40 tests, production build, Rustfmt, strict Clippy
  และ Rust test matrix ค่ะ
- `:tauri-plugin-vault-saf:testDebugUnitTest` ผ่านบน generated Android host ค่ะ
- `git diff --check` ผ่านค่ะ
- การตรวจชื่อไฟล์และรูปแบบ Secret พื้นฐานไม่พบ Refresh token, Client secret,
  Private key หรือ Google API key ในงานที่เพิ่มเข้ามาค่ะ เครื่องไม่มี Gitleaks จึง
  ไม่ถือเป็น formal secret scan เต็มรูปแบบค่ะ
- Live Google Drive, personal Vault และ OS credential store ไม่ถูกเรียกระหว่าง
  archive verification ค่ะ

## สิ่งที่ยังไม่เสร็จ

- R3 Safe Conflict Core ยังไม่ผ่าน Gate 4, Gate 5, orchestration และ live
  two-device acceptance ค่ะ
- Sync control-plane UI, remaining local product journey, knowledge index และ
  cross-platform release acceptance ยังไม่เสร็จค่ะ
- Existing-item remote mutation ยังคง fail closed และไม่มีสิทธิ์เรียกโครงการนี้ว่า
  production-ready ค่ะ

## วิธีนำ Source กลับมา

```bash
git clone https://github.com/abhuri/myVault.git
cd myVault
git fetch --tags origin
git checkout codex/r3-4-completion
git status --short --branch
git tag --verify paused-2026-07-18-r3-4
```

จากนั้นให้อ่านไฟล์นี้และ `SESSION_HANDOFF.md` ก่อนทำงานค่ะ หากต้องการ baseline ที่
เสถียรกว่า incomplete R3 candidate ให้ checkout `main` ค่ะ

## Resume Gate

ห้ามเริ่ม implementation ต่อจาก checkpoint นี้จนกว่าจะมีเหตุผลที่พิสูจน์จากการใช้
Obsidian/Google Drive Sync จริง, กำหนดช่องว่างที่ต้องแก้, ทบทวน dependency/security
ที่เปลี่ยนไประหว่างพัก และได้รับ approval สำหรับ scope ใหม่ค่ะ

## ข้อมูลที่ไม่อยู่ใน GitHub

Runtime state, recovery snapshots, Google OAuth credential, personal Vault และไฟล์
ผู้ใช้ไม่อยู่ใน Repository ค่ะ Source-only archive ไม่ได้ลบหรือย้ายข้อมูลเหล่านั้นค่ะ
