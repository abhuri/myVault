---
title: Local-first Safety
tags: [architecture, safety]
---

# Local-first Safety

หลัก `local-first` เปรียบเหมือนสมุดจริงอยู่กับเรา ส่วน cloud เป็นพนักงานส่งสำเนา ไม่ใช่เจ้าของต้นฉบับค่ะ

## กติกาหลัก

- Markdown และ attachment เป็น source of truth
- บันทึกแบบ atomic และตรวจ revision ก่อนเขียน
- เมื่อ revision ไม่ตรง ให้แจ้ง conflict และหยุดเขียน
- `.trash/` และ `.obsidian/` ไม่อยู่ใน explorer ปกติ

Related: [[Projects/myVault Demo]] · [[Notes/ภาษาไทยและ Unicode]]

