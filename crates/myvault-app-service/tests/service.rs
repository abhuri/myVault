use myvault_app_service::{
    AppError, AppErrorCode, AppService, ExplorerKindDto, NoteDto, SaveDurabilityDto,
    TrashEvidenceDto, TrashItemDto, TrashPageDto, VaultSessionId, VaultStatusDto,
    EXPLORER_DEFAULT_PAGE_SIZE, EXPLORER_MAX_DEPTH, EXPLORER_MAX_PAGE_SIZE, EXPLORER_MAX_SCAN,
};
use myvault_core::{FileRevision, TrashId, TrashManifestV1, Vault, VaultPath};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

struct Fixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let temporary = tempfile::tempdir().expect("temporary");
        let root = temporary.path().join(label);
        fs::create_dir(&root).expect("root");
        let root = root.canonicalize().expect("canonical root");
        Self {
            _temporary: temporary,
            root,
        }
    }

    fn write(&self, path: &str, bytes: &[u8]) {
        let target = self.root.join(path);
        fs::create_dir_all(target.parent().expect("parent")).expect("parents");
        fs::write(target, bytes).expect("write");
    }
}

fn activate(service: &AppService, fixture: &Fixture) -> VaultSessionId {
    let vault = Vault::open(&fixture.root).expect("trusted native picker opens vault");
    service
        .activate_trusted_vault(vault)
        .expect("activate")
        .session_id
        .expect("session id")
}

#[test]
fn canonical_session_input_and_camel_case_json_contract_are_exact() {
    let text = "12345678-1234-4abc-8def-1234567890ab";
    let session = VaultSessionId::parse(text).expect("canonical session");
    assert_eq!(session.to_string(), text);
    assert_eq!(
        serde_json::from_str::<VaultSessionId>(&format!("\"{text}\""))
            .expect("deserialize canonical"),
        session
    );
    for invalid in [
        "12345678-1234-4ABC-8DEF-1234567890AB",
        "1234567812344abc8def1234567890ab",
        "00000000-0000-0000-0000-000000000000",
        "/private/ambient/vault",
    ] {
        assert!(VaultSessionId::parse(invalid).is_err());
        assert!(serde_json::from_str::<VaultSessionId>(&format!("\"{invalid}\"")).is_err());
    }
    assert_eq!(
        VaultSessionId::parse("NOT-A-SESSION")
            .expect_err("invalid session")
            .code,
        AppErrorCode::InvalidSessionId
    );
    assert_eq!(
        serde_json::to_string(&VaultStatusDto {
            active: true,
            session_id: Some(session),
        })
        .expect("status JSON"),
        format!("{{\"active\":true,\"sessionId\":\"{text}\"}}")
    );
    assert_eq!(
        serde_json::to_string(&NoteDto {
            session_id: session,
            path: "note.md".to_owned(),
            text: "ไทย".to_owned(),
            revision_hex: "a".repeat(64),
            byte_len: 9,
        })
        .expect("note JSON"),
        format!(
            "{{\"sessionId\":\"{text}\",\"path\":\"note.md\",\"text\":\"ไทย\",\"revisionHex\":\"{}\",\"byteLen\":9}}",
            "a".repeat(64)
        )
    );
    let supported = TrashEvidenceDto::Supported {
        original_path: "โน้ต.md".to_owned(),
        trashed_at_unix_ms: 7,
        revision_hex: "b".repeat(64),
        byte_len: 12,
        manifest_digest: "c".repeat(64),
    };
    assert_eq!(
        serde_json::to_string(&supported).expect("supported evidence JSON"),
        format!(
            "{{\"kind\":\"supported\",\"originalPath\":\"โน้ต.md\",\"trashedAtUnixMs\":7,\"revisionHex\":\"{}\",\"byteLen\":12,\"manifestDigest\":\"{}\"}}",
            "b".repeat(64),
            "c".repeat(64)
        )
    );
    assert_eq!(
        serde_json::to_string(&TrashEvidenceDto::Opaque).expect("opaque evidence JSON"),
        "{\"kind\":\"opaque\"}"
    );
    let page_json = serde_json::to_string(&TrashPageDto {
        session_id: session,
        entries: vec![TrashItemDto {
            trash_id: "10000000-0000-4000-8000-000000000001".to_owned(),
            evidence: supported,
        }],
        invalid_name_count: 1,
        next_after: None,
        has_more: false,
        scanned_entries: 2,
    })
    .expect("page JSON");
    for key in [
        "sessionId",
        "trashId",
        "originalPath",
        "trashedAtUnixMs",
        "revisionHex",
        "byteLen",
        "manifestDigest",
        "invalidNameCount",
        "nextAfter",
        "hasMore",
        "scannedEntries",
    ] {
        assert!(page_json.contains(&format!("\"{key}\"")));
    }
    let service = AppService::new();
    let error = service
        .read_note(session, "note.md")
        .expect_err("no active session");
    assert_eq!(
        serde_json::to_string(&error).expect("error JSON"),
        "{\"code\":\"noActiveSession\",\"message\":\"no vault session is active\"}"
    );
}

#[test]
fn no_stale_switched_and_closed_sessions_are_rejected() {
    let service = AppService::new();
    let first = Fixture::new("first-vault-secret");
    let second = Fixture::new("second-vault-secret");
    first.write("note.md", b"first");
    second.write("note.md", b"second");

    let no_session = service
        .read_note(dummy_session_id(), "note.md")
        .expect_err("no session");
    assert_eq!(no_session.code, AppErrorCode::NoActiveSession);

    let first_id = activate(&service, &first);
    let second_id = activate(&service, &second);
    assert_ne!(first_id, second_id);
    assert_eq!(
        service
            .read_note(first_id, "note.md")
            .expect_err("stale read")
            .code,
        AppErrorCode::StaleSession
    );
    assert_eq!(
        service.close(first_id).expect_err("stale close").code,
        AppErrorCode::StaleSession
    );
    assert_eq!(
        service
            .read_note(second_id, "note.md")
            .expect("current read")
            .text,
        "second"
    );
    service.close(second_id).expect("close current");
    assert!(!service.status().expect("status").active);
}

#[test]
fn explorer_is_sorted_bounded_exclusive_and_path_private() {
    assert_eq!(EXPLORER_MAX_DEPTH, 64);
    assert_eq!(EXPLORER_MAX_SCAN, 5_000);
    assert_eq!(EXPLORER_DEFAULT_PAGE_SIZE, 100);
    assert_eq!(EXPLORER_MAX_PAGE_SIZE, 200);
    let service = AppService::new();
    let fixture = Fixture::new("explorer-root-secret");
    fixture.write("zeta.txt", b"z");
    fixture.write("folder/ไทย.md", "ไทย".as_bytes());
    fixture.write("alpha.MD", b"a");
    fixture.write(".obsidian/private.json", b"{}");
    fixture.write(".trash/v1/items/hidden", b"");
    let session = activate(&service, &fixture);

    for limit in [0, 201] {
        assert_eq!(
            service
                .list_explorer(session, None, limit)
                .expect_err("invalid limit")
                .code,
            AppErrorCode::InvalidLimit
        );
    }
    for cursor in ["./alpha.MD", "folder//ไทย.md", "../escape.md"] {
        assert_eq!(
            service
                .list_explorer(session, Some(cursor), 1)
                .expect_err("noncanonical cursor")
                .code,
            AppErrorCode::InvalidCursor
        );
    }

    let first = service.list_explorer(session, None, 2).expect("first page");
    assert_eq!(
        first
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        ["alpha.MD", "folder/ไทย.md"]
    );
    assert!(first
        .entries
        .iter()
        .all(|entry| entry.kind == ExplorerKindDto::Markdown));
    assert!(first.has_more);
    assert_eq!(first.scanned_entries, 3);
    let second = service
        .list_explorer(
            session,
            first.next_after.as_deref(),
            EXPLORER_DEFAULT_PAGE_SIZE,
        )
        .expect("exclusive second page");
    assert_eq!(second.entries.len(), 1);
    assert_eq!(second.entries[0].path, "zeta.txt");
    assert_eq!(second.entries[0].kind, ExplorerKindDto::File);
    assert!(!second.has_more);
    let json = serde_json::to_string(&first).expect("explorer JSON");
    assert!(json.contains("\"sessionId\""));
    assert!(json.contains("\"byteLen\""));
    assert!(json.contains("\"kind\":\"markdown\""));
    assert!(!json.contains(fixture.root.to_str().expect("UTF-8 root")));
    assert!(!json.contains("explorer-root-secret"));

    let switched = Fixture::new("switched-explorer-root");
    activate(&service, &switched);
    assert_eq!(
        service
            .list_explorer(session, None, 1)
            .expect_err("stale explorer session")
            .code,
        AppErrorCode::StaleSession
    );
}

#[test]
fn thai_markdown_and_uppercase_md_round_trip_with_exact_revision() {
    let service = AppService::new();
    let fixture = Fixture::new("thai-root-secret");
    let bytes = "# สวัสดี\nบันทึกภาษาไทย 🪷".as_bytes();
    fixture.write("บันทึก.MD", bytes);
    let session = activate(&service, &fixture);

    let note = service.read_note(session, "บันทึก.MD").expect("Thai note");
    assert_eq!(note.text.as_bytes(), bytes);
    assert_eq!(note.byte_len, bytes.len() as u64);
    assert_eq!(note.revision_hex, FileRevision::from_bytes(bytes).hex);
    let note_json = serde_json::to_string(&note).expect("note JSON");
    assert!(!note_json.contains(fixture.root.to_str().expect("UTF-8 root")));
    assert!(!note_json.contains("thai-root-secret"));
    assert_eq!(
        service
            .read_note(session, "บันทึก.Md")
            .expect_err("unsupported extension casing")
            .code,
        AppErrorCode::InvalidPath
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn save_note_is_revision_checked_bounded_utf8_and_frontend_safe() {
    let service = AppService::new();
    let fixture = Fixture::new("save-root-secret");
    let initial = "เดิม".as_bytes();
    fixture.write("บันทึก.md", initial);
    let session = activate(&service, &fixture);
    let expected = FileRevision::from_bytes(initial);
    let replacement = "เนื้อหาใหม่ 🪷";
    let saved = service
        .save_note(
            session,
            "บันทึก.md",
            replacement,
            &expected.hex,
            expected.byte_len,
        )
        .expect("save Thai note");
    assert_eq!(saved.path, "บันทึก.md");
    assert_eq!(saved.byte_len, replacement.len() as u64);
    assert_eq!(
        saved.revision_hex,
        FileRevision::from_bytes(replacement.as_bytes()).hex
    );
    assert!(matches!(
        saved.durability,
        SaveDurabilityDto::FullySynced | SaveDurabilityDto::DirectorySyncUnsupported
    ));
    assert_eq!(
        fs::read(fixture.root.join("บันทึก.md")).expect("saved disk bytes"),
        replacement.as_bytes()
    );
    assert_eq!(
        service
            .read_note(session, "บันทึก.md")
            .expect("round trip")
            .text,
        replacement
    );
    let json = serde_json::to_string(&saved).expect("save JSON");
    for key in ["sessionId", "path", "revisionHex", "byteLen", "durability"] {
        assert!(json.contains(&format!("\"{key}\"")));
    }
    assert!(!json.contains(fixture.root.to_str().expect("UTF-8 root")));
    assert!(!json.contains("save-root-secret"));
    assert!(json.contains(match saved.durability {
        SaveDurabilityDto::FullySynced => "\"durability\":\"fullySynced\"",
        SaveDurabilityDto::DirectorySyncUnsupported => {
            "\"durability\":\"directorySyncUnsupported\""
        }
    }));

    let disk_before_stale = fs::read(fixture.root.join("บันทึก.md")).expect("before stale");
    let stale = service
        .save_note(
            session,
            "บันทึก.md",
            "must not land",
            &expected.hex,
            expected.byte_len,
        )
        .expect_err("stale revision");
    assert_eq!(stale.code, AppErrorCode::StaleRevision);
    assert_eq!(
        fs::read(fixture.root.join("บันทึก.md")).expect("after stale"),
        disk_before_stale
    );
    assert!(serde_json::to_string(&stale)
        .expect("stale JSON")
        .contains("\"code\":\"staleRevision\""));
    assert_eq!(
        service
            .save_note(session, "บันทึก.md", "x", "INVALID", 1)
            .expect_err("invalid revision")
            .code,
        AppErrorCode::InvalidRevision
    );
    for path in ["note.txt", "../escape.md", ".trash/note.md"] {
        let error = service
            .save_note(session, path, "x", &saved.revision_hex, saved.byte_len)
            .expect_err("invalid save path");
        assert_eq!(
            error.code,
            AppErrorCode::InvalidPath,
            "unexpected code for {path}"
        );
    }
    let oversized = "x".repeat(myvault_core::MAX_NOTE_BYTES + 1);
    assert_eq!(
        service
            .save_note(
                session,
                "บันทึก.md",
                &oversized,
                &saved.revision_hex,
                saved.byte_len,
            )
            .expect_err("oversized")
            .code,
        AppErrorCode::ResourceLimit
    );
    for error in [stale, AppError::internal()] {
        let error_json = serde_json::to_string(&error).expect("error JSON");
        assert!(!error_json.contains("save-root-secret"));
        assert!(!error_json.contains(fixture.root.to_str().expect("UTF-8 root")));
        assert!(!error_json.contains("filesystem"));
    }
}

#[test]
fn invalid_utf8_and_serialized_errors_never_leak_root_or_os_details() {
    let service = AppService::new();
    let fixture = Fixture::new("absolute-root-must-never-leak");
    fixture.write("broken.md", &[0xff, 0xfe]);
    let session = activate(&service, &fixture);
    let error = service
        .read_note(session, "broken.md")
        .expect_err("invalid UTF-8");
    assert_eq!(error.code, AppErrorCode::NoteNotUtf8);

    for error in [
        error,
        service
            .read_note(session, "missing.md")
            .expect_err("missing"),
        service
            .read_note(session, "../escape.md")
            .expect_err("invalid path"),
    ] {
        let json = serde_json::to_string(&error).expect("safe error JSON");
        assert!(!json.contains(fixture.root.to_str().expect("UTF-8 root")));
        assert!(!json.contains("absolute-root-must-never-leak"));
        assert!(!json.contains("No such file"));
        assert!(!json.contains("backtrace"));
    }
}

#[test]
fn trash_mapping_pagination_limits_cursor_casing_and_root_privacy_are_safe() {
    let service = AppService::new();
    let fixture = Fixture::new("trash-root-secret");
    let first = TrashId::parse("10000000-0000-4000-8000-000000000001").expect("first id");
    let second = TrashId::parse("20000000-0000-4000-8000-000000000002").expect("second id");
    write_supported_trash(&fixture.root, first, "โน้ต.md", b"Thai payload", 10);
    write_opaque_trash(&fixture.root, second);
    fs::create_dir_all(fixture.root.join(".trash/v1/items/not-a-uuid"))
        .expect("invalid trash name");
    let session = activate(&service, &fixture);

    assert_eq!(
        service
            .list_trash(session, None, 0)
            .expect_err("zero limit")
            .code,
        AppErrorCode::InvalidLimit
    );
    assert_eq!(
        service
            .list_trash(session, None, 101)
            .expect_err("large limit")
            .code,
        AppErrorCode::InvalidLimit
    );
    assert_eq!(
        service
            .list_trash(session, Some("10000000-0000-4000-8000-00000000000A"), 1,)
            .expect_err("uppercase cursor")
            .code,
        AppErrorCode::InvalidCursor
    );

    let page_one = service.list_trash(session, None, 1).expect("page one");
    assert_eq!(page_one.entries.len(), 1);
    assert_eq!(page_one.entries[0].trash_id, first.to_string());
    assert!(matches!(
        &page_one.entries[0].evidence,
        TrashEvidenceDto::Supported { original_path, .. } if original_path == "โน้ต.md"
    ));
    assert!(page_one.has_more);
    let cursor = page_one.next_after.as_deref().expect("cursor");
    let page_two = service
        .list_trash(session, Some(cursor), 1)
        .expect("page two");
    assert_eq!(page_two.entries[0].trash_id, second.to_string());
    assert!(matches!(
        page_two.entries[0].evidence,
        TrashEvidenceDto::Opaque
    ));
    assert!(page_two.invalid_name_count >= 1);

    for json in [
        serde_json::to_string(&service.status().expect("status")).expect("status JSON"),
        serde_json::to_string(&page_one).expect("page JSON"),
        serde_json::to_string(&page_two).expect("page JSON"),
    ] {
        assert!(!json.contains(fixture.root.to_str().expect("UTF-8 root")));
        assert!(!json.contains("trash-root-secret"));
    }
}

#[test]
fn trash_scan_resource_limit_uses_stable_safe_code() {
    let service = AppService::new();
    let fixture = Fixture::new("bounded-trash-root-secret");
    let items = fixture.root.join(".trash/v1/items");
    fs::create_dir_all(&items).expect("items");
    for index in 0..=myvault_core::MAX_TRASH_LIST_SCAN {
        fs::write(items.join(format!("invalid-{index}")), b"").expect("invalid entry");
    }
    let session = activate(&service, &fixture);
    let error = service
        .list_trash(session, None, 1)
        .expect_err("scan limit");
    assert_eq!(error.code, AppErrorCode::ResourceLimit);
    let json = serde_json::to_string(&error).expect("safe error");
    assert!(!json.contains(fixture.root.to_str().expect("UTF-8 root")));
    assert!(!json.contains("bounded-trash-root-secret"));
}

fn write_supported_trash(
    root: &Path,
    trash_id: TrashId,
    original: &str,
    payload: &[u8],
    trashed_at: i64,
) {
    let manifest = TrashManifestV1::new(
        trash_id,
        Uuid::new_v4(),
        &VaultPath::from_portable(original).expect("original path"),
        FileRevision::from_bytes(payload),
        trashed_at,
    )
    .expect("manifest");
    let item = root.join(".trash/v1/items").join(trash_id.to_string());
    fs::create_dir_all(&item).expect("item");
    fs::write(
        item.join("manifest.json"),
        manifest.canonical_bytes().expect("canonical manifest"),
    )
    .expect("manifest");
    fs::write(item.join("payload"), payload).expect("payload");
}

fn write_opaque_trash(root: &Path, trash_id: TrashId) {
    let item = root.join(".trash/v1/items").join(trash_id.to_string());
    fs::create_dir_all(&item).expect("item");
    fs::write(item.join("manifest.json"), b"{\"version\":999}").expect("future manifest");
    fs::write(item.join("payload"), b"opaque").expect("payload");
}

fn dummy_session_id() -> VaultSessionId {
    let service = AppService::new();
    let fixture = Fixture::new("dummy");
    activate(&service, &fixture)
}
