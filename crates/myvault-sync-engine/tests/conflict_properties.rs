use myvault_sync_engine::conflict::{
    classify_conflict, derive_conflict_id, derive_operation_id, materialize_conflict_plan,
    normalized_collision_key, resolve_conflict_copy_name, ClassificationEvidence,
    ClassificationEvidenceFacts, ConflictCase, ConflictCopyNameOutcome, ConflictCopyNameRequest,
    ConflictIdentityInput, ConflictInput, ConflictNameFailure, ConflictOperationDomain,
    ContentFingerprint, LocalPlanKind, MaterializationContext, OccupiedConflictCopy,
    PublicationSource, PublicationTarget, CONFLICT_NAMING_VERSION,
};

fn fingerprint(byte: u8) -> ContentFingerprint {
    ContentFingerprint {
        sha256: format!("{byte:02x}").repeat(32),
        byte_length: u64::from(byte),
    }
}

fn identity_input() -> ConflictIdentityInput {
    ConflictIdentityInput {
        account_id: "account-1".to_owned(),
        remote_root_id: "root-1".to_owned(),
        object_identity: "object-1".to_owned(),
        cell: myvault_sync_engine::conflict::ConflictCell::C05,
        outcome: myvault_sync_engine::conflict::ConflictOutcome::PreserveBothLocal,
        canonical_identity_path: "notes/alpha.md".to_owned(),
        target_parent_id: "parent-1".to_owned(),
        base: Some(fingerprint(0x11)),
        local: Some(fingerprint(0x22)),
        remote: Some(fingerprint(0x33)),
        naming_version: CONFLICT_NAMING_VERSION.to_owned(),
    }
}

fn name_request(source_path: impl Into<String>) -> ConflictCopyNameRequest {
    ConflictCopyNameRequest {
        conflict_id: "0123456789abcdef".repeat(4),
        source_path: source_path.into(),
        destination_parent_path: Some("notes".to_owned()),
        expected_content: fingerprint(0x44),
        naming_version: CONFLICT_NAMING_VERSION.to_owned(),
        occupied: Vec::new(),
    }
}

#[test]
fn conflict_identity_and_operation_domains_match_frozen_golden_values() {
    let input = identity_input();
    let first = derive_conflict_id(&input).expect("valid identity evidence");
    let second = derive_conflict_id(&input).expect("deterministic rerun");
    assert_eq!(first, second);
    assert_eq!(
        first,
        "825d95413b62dabd5d6c23d9d205df76176eead3f0c96562a66bf9b822977ea9"
    );

    let golden = [
        (
            ConflictOperationDomain::ConflictCopy,
            "4ba68ae8-cdf9-59d4-94b2-830644de80e9",
        ),
        (
            ConflictOperationDomain::MergePublish,
            "b8353b6a-ddb4-54cd-ac64-60f12b6d26b2",
        ),
        (
            ConflictOperationDomain::BasePublish,
            "d34d2658-450c-5e1f-98aa-7b1de28530c3",
        ),
        (
            ConflictOperationDomain::LocalPublish,
            "4ef5c33a-6f00-5f6d-94a3-3d31a703d80a",
        ),
        (
            ConflictOperationDomain::RemoteExistingBlocked,
            "e63cf237-cf7c-540b-bd75-3dc393164e06",
        ),
        (
            ConflictOperationDomain::GuardedLocalRename,
            "569d3455-1980-5067-8371-d22e271f7ede",
        ),
        (
            ConflictOperationDomain::GuardedLocalMove,
            "fd27f02f-27a8-5553-b1ad-54a210088c98",
        ),
        (
            ConflictOperationDomain::GuardedLocalRenameMove,
            "f8337530-4de3-57a9-bab6-658759c3e0b3",
        ),
    ];
    for (domain, expected) in golden {
        assert_eq!(derive_operation_id(domain, &first).to_string(), expected);
    }
    assert_ne!(
        derive_operation_id(ConflictOperationDomain::ConflictCopy, &first),
        derive_operation_id(ConflictOperationDomain::MergePublish, &first)
    );
}

#[test]
fn length_delimited_identity_fields_resist_concat_and_optional_value_ambiguity() {
    let original = identity_input();

    let mut repartitioned = original.clone();
    repartitioned.account_id = "account-1root".to_owned();
    repartitioned.remote_root_id = "-1".to_owned();
    assert_eq!(
        format!("{}{}", original.account_id, original.remote_root_id),
        format!(
            "{}{}",
            repartitioned.account_id, repartitioned.remote_root_id
        )
    );
    assert_ne!(
        derive_conflict_id(&original).expect("original"),
        derive_conflict_id(&repartitioned).expect("repartitioned")
    );

    let mut no_base = original.clone();
    no_base.base = None;
    assert_ne!(
        derive_conflict_id(&original).expect("present base"),
        derive_conflict_id(&no_base).expect("absent base")
    );
}

#[test]
fn identity_input_is_complete_without_clock_device_or_arrival_metadata() {
    // Constructing the public evidence type from immutable sync evidence alone freezes the
    // contract: wall-clock timestamps, device labels, and arrival ordering are not inputs.
    let input = identity_input();
    assert_eq!(derive_conflict_id(&input).expect("identity").len(), 64);
}

#[test]
fn collision_keys_apply_nfkc_full_casefold_and_reject_normalized_slashes() {
    let ascii = normalized_collision_key("notes/strasse/a.md").expect("ASCII key");
    let compatibility =
        normalized_collision_key("ｎｏｔｅｓ/STRAßE/Ａ.md").expect("compatibility key");
    assert_eq!(ascii, compatibility);

    assert_eq!(
        normalized_collision_key("notes/fullwidth／slash.md"),
        Err(ConflictNameFailure::InvalidNormalizedComponent)
    );
}

#[test]
fn naming_reuses_only_exact_evidence_and_expands_prefix_by_four_on_collision() {
    let request = name_request("notes/alpha.md");
    let ConflictCopyNameOutcome::Create(first) = resolve_conflict_copy_name(&request) else {
        panic!("unoccupied name must be created");
    };
    assert_eq!(first.id_prefix_length, 12);
    assert!(first
        .destination_path
        .ends_with("alpha (conflict 0123456789ab).md"));

    let mut exact = request.clone();
    exact.occupied.push(OccupiedConflictCopy {
        normalized_collision_key: first.normalized_collision_key.clone(),
        conflict_id: request.conflict_id.clone(),
        expected_content: request.expected_content.clone(),
        destination_path: first.destination_path.clone(),
        object_id: "object-existing".to_owned(),
    });
    let ConflictCopyNameOutcome::Reuse(reused) = resolve_conflict_copy_name(&exact) else {
        panic!("exact evidence must be reused");
    };
    assert_eq!(reused.destination_path, first.destination_path);
    assert_eq!(
        reused.existing_object_id.as_deref(),
        Some("object-existing")
    );

    let mut mismatched = request.clone();
    mismatched.occupied.push(OccupiedConflictCopy {
        normalized_collision_key: first.normalized_collision_key,
        conflict_id: request.conflict_id.clone(),
        expected_content: fingerprint(0x45),
        destination_path: first.destination_path,
        object_id: "object-collision".to_owned(),
    });
    let ConflictCopyNameOutcome::Create(expanded) = resolve_conflict_copy_name(&mismatched) else {
        panic!("mismatched evidence must not be reused");
    };
    assert_eq!(expanded.id_prefix_length, 16);
    assert!(expanded
        .destination_path
        .ends_with("alpha (conflict 0123456789abcdef).md"));
}

#[test]
fn unicode_stem_truncation_preserves_scalar_boundaries_and_portable_component_limits() {
    // 50 four-byte scalars fit as a source component but require truncation once the suffix is
    // added. Exercise each deterministic prefix length without randomized input.
    for collision_count in 0..=13 {
        let mut request = name_request(format!("notes/{}.md", "📝".repeat(50)));
        for _ in 0..collision_count {
            let ConflictCopyNameOutcome::Create(candidate) = resolve_conflict_copy_name(&request)
            else {
                panic!("intermediate candidate");
            };
            request.occupied.push(OccupiedConflictCopy {
                normalized_collision_key: candidate.normalized_collision_key,
                conflict_id: "f".repeat(64),
                expected_content: fingerprint(0x55),
                destination_path: candidate.destination_path,
                object_id: "object-collision".to_owned(),
            });
        }
        let ConflictCopyNameOutcome::Create(name) = resolve_conflict_copy_name(&request) else {
            panic!("bounded prefix search must find a portable name");
        };
        assert_eq!(name.id_prefix_length, 12 + (4 * collision_count));
        let component = name.destination_path.rsplit('/').next().expect("component");
        assert!(component.len() <= 255);
        assert!(component.is_char_boundary(component.len()));
        assert!(!component.contains('�'));
    }
}

fn source(name: &str, byte: u8) -> PublicationSource {
    PublicationSource {
        portable_path: format!("notes/{name}.md"),
        parent_id: "parent-1".to_owned(),
        object_id: format!("object-{name}"),
        revision: Some(format!("{byte:02x}").repeat(32)),
        content: fingerprint(byte),
    }
}

fn blocked_context() -> MaterializationContext {
    MaterializationContext {
        account_id: "account-1".to_owned(),
        remote_root_id: "root-1".to_owned(),
        base_reference: Some("base-1".to_owned()),
        durable_state_version: 1,
        base_local_revision: Some("11".repeat(32)),
        base_remote_revision: Some("base-remote-revision".to_owned()),
        base: Some(source("base", 1)),
        local: Some(source("local", 2)),
        remote: Some(source("remote", 1)),
        local_target: PublicationTarget {
            portable_path: "notes/local.md".to_owned(),
            parent_id: "parent-1".to_owned(),
            object_id: Some("object-local".to_owned()),
        },
        conflict_copy_destination_parent_path: None,
        occupied_conflict_copies: Vec::new(),
        rename_target: None,
        blocked_remote_source: Some(source("local", 2)),
        blocked_remote_destination: Some(PublicationTarget {
            portable_path: "notes/local.md".to_owned(),
            parent_id: "parent-1".to_owned(),
            object_id: Some("object-local".to_owned()),
        }),
        merged_content: None,
    }
}

fn classification_evidence() -> ClassificationEvidence {
    ClassificationEvidence::new(ClassificationEvidenceFacts {
        account_id: Some("account-1".to_owned()),
        remote_root_id: Some("root-1".to_owned()),
        object_identity: Some("object-base".to_owned()),
        canonical_identity_path: Some("notes/base.md".to_owned()),
        target_parent_id: Some("parent-1".to_owned()),
        base: Some(fingerprint(1)),
        local: Some(fingerprint(2)),
        remote: Some(fingerprint(1)),
        local_revision: Some("02".repeat(32)),
        remote_revision: Some("01".repeat(32)),
        durable_state_version: 1,
        prior_operation_id: None,
        prior_outcome_fingerprint: None,
        prior_intent_fingerprint: None,
        prior_replay_disposition: None,
        guarded_metadata_target: None,
    })
}

#[test]
fn remote_blocked_draft_is_excluded_from_cursor_dag_and_reruns_stably() {
    let classification = classify_conflict(
        ConflictInput::fresh(classification_evidence(), ConflictCase::LocalContentChanged)
            .expect("classification input"),
    );
    let context = blocked_context();
    let first = materialize_conflict_plan(&classification, &context).expect("first plan");
    let second = materialize_conflict_plan(&classification, &context).expect("stable rerun");

    assert_eq!(first, second);
    assert!(first.cursor_dependencies().is_empty());
    assert_eq!(first.drafts().len(), 1);
    assert_eq!(
        first.drafts()[0].kind(),
        LocalPlanKind::RemoteExistingBlocked
    );
    assert!(first
        .cursor_dependencies()
        .iter()
        .all(|dependency| dependency.operation_id != first.drafts()[0].operation_id()));
}
