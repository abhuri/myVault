use myvault_sync_engine::conflict::{
    classify_conflict, normalized_collision_key, BoundaryAssessment, ClassificationEvidence,
    ClassificationEvidenceFacts, ConflictCase, ConflictCell, ConflictDraft, ConflictInput,
    ConflictInputFailure, ConflictOutcome, ContentFingerprint, CursorGate, DeletedFinalStateProof,
    GuardedLocalCapability, GuardedMetadataTargetEvidence, LocalRenameGuard, ReplayAssessment,
    ReplayDisposition, ReplayProof, RetainedEvidence, VerifiedNotAppliedRetry,
};
use myvault_sync_engine::{
    MutationDisposition, MutationEvidenceCapturePhase, MutationIntent, MutationOperationKind,
    MutationPhase, MutationState, MutationVerificationEvidence,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

fn content_fingerprint(content: &str) -> ContentFingerprint {
    ContentFingerprint {
        sha256: format!("{:x}", Sha256::digest(content.as_bytes())),
        byte_length: u64::try_from(content.len()).expect("fixture length fits u64"),
    }
}

fn exact_facts() -> ClassificationEvidenceFacts {
    ClassificationEvidenceFacts {
        account_id: Some("account-1".to_owned()),
        remote_root_id: Some("root-1".to_owned()),
        object_identity: Some("object-1".to_owned()),
        canonical_identity_path: Some("notes/a.md".to_owned()),
        target_parent_id: Some("parent-1".to_owned()),
        base: Some(content_fingerprint("a\nb\n")),
        local: Some(content_fingerprint("A\nb\n")),
        remote: Some(content_fingerprint("a\nB\n")),
        local_revision: Some("11".repeat(32)),
        remote_revision: Some("remote-revision-1".to_owned()),
        durable_state_version: 2,
        prior_operation_id: None,
        prior_outcome_fingerprint: None,
        prior_intent_fingerprint: None,
        prior_replay_disposition: None,
        guarded_metadata_target: Some(GuardedMetadataTargetEvidence {
            portable_path: "notes/guarded.md".to_owned(),
            parent_id: "parent-1".to_owned(),
            object_id: Some("object-1".to_owned()),
            normalized_collision_key: normalized_collision_key("notes/guarded.md")
                .expect("guarded collision key"),
            occupied_collision_keys: Vec::new(),
        }),
    }
}

fn exact_evidence() -> ClassificationEvidence {
    semantic_evidence(
        ClassificationEvidence::new(exact_facts())
            .verify_markdown_merge("a\nb\n", "A\nb\n", "a\nB\n")
            .expect("verified non-overlap merge"),
    )
}

fn semantic_evidence(evidence: ClassificationEvidence) -> ClassificationEvidence {
    evidence
        .verify_deleted_final_state("account-1", "root-1", "object-1", 2, true, true)
        .expect("verified deleted state")
        .verify_same_rename(
            "account-1",
            "root-1",
            "object-1",
            2,
            "notes/same.md",
            "notes/same.md",
        )
        .expect("verified same rename")
        .verify_same_move("account-1", "root-1", "object-1", 2, "parent-1", "parent-1")
        .expect("verified same move")
}

fn input(case: ConflictCase) -> ConflictInput {
    let mut facts = exact_facts();
    match case {
        ConflictCase::RemoteContentChanged { .. } => facts.local.clone_from(&facts.base),
        ConflictCase::LocalContentChanged => facts.remote.clone_from(&facts.base),
        ConflictCase::BothChangedWithoutExactBase { .. } => facts.base = None,
        _ => {}
    }
    let mut evidence = ClassificationEvidence::new(facts);
    if case == ConflictCase::NonOverlappingMarkdownChanges {
        evidence = evidence
            .verify_markdown_merge("a\nb\n", "A\nb\n", "a\nB\n")
            .expect("verified non-overlap merge");
    }
    evidence = semantic_evidence(evidence);
    ConflictInput::fresh(evidence, case).expect("exact test evidence")
}

fn replay_proof(disposition: ReplayDisposition, kind: MutationOperationKind) -> ReplayProof {
    let mut intent = MutationIntent {
        operation_id: Uuid::from_u128(1),
        operation_kind: kind,
        account_id: Some("account-1".to_owned()),
        remote_root_id: Some("root-1".to_owned()),
        remote_file_id: Some("object-1".to_owned()),
        source_parent_id: Some("parent-1".to_owned()),
        destination_parent_id: Some("parent-1".to_owned()),
        local_object_id: None,
        source_path: Some("notes/a.md".to_owned()),
        destination_path: Some("notes/a.md".to_owned()),
        expected_local_revision: Some("11".repeat(32)),
        expected_remote_revision: Some("remote-revision-1".to_owned()),
        base_reference: None,
        base_local_revision: None,
        base_remote_revision: None,
        base_sha256: Some(content_fingerprint("a\nb\n").sha256),
        base_byte_length: Some(4),
        expected_local_sha256: Some(content_fingerprint("a\nB\n").sha256),
        expected_local_byte_length: Some(4),
        expected_remote_sha256: Some(content_fingerprint("a\nB\n").sha256),
        expected_remote_byte_length: Some(4),
        operation_marker: "r3.fixture".to_owned(),
        intent_fingerprint: String::new(),
        registered_at_unix_ms: 1,
    };
    if disposition == ReplayDisposition::QueuedIntentCapturedBaseChanged {
        intent.base_sha256 = Some("ff".repeat(32));
        intent.intent_fingerprint = intent.canonical_fingerprint();
        return ReplayProof::from_queued_base_change(&intent, 1, &exact_facts())
            .expect("queued base changed proof");
    }
    intent.intent_fingerprint = intent.canonical_fingerprint();
    let mutation_disposition = match disposition {
        ReplayDisposition::VerifiedAppliedExactPostState => MutationDisposition::VerifiedApplied,
        ReplayDisposition::VerifiedNotApplied => MutationDisposition::VerifiedNotApplied,
        ReplayDisposition::SideEffectOutcomeUnknown => MutationDisposition::NeedsReconcile,
        ReplayDisposition::QueuedIntentCapturedBaseChanged => unreachable!("handled above"),
    };
    let capture_phase = if disposition == ReplayDisposition::SideEffectOutcomeUnknown {
        MutationEvidenceCapturePhase::Reconcile
    } else {
        MutationEvidenceCapturePhase::PostVerify
    };
    let mut evidence = MutationVerificationEvidence {
        evidence_id: Uuid::from_u128(2),
        operation_id: intent.operation_id,
        attempt_number: 1,
        capture_phase,
        disposition: mutation_disposition,
        outcome_code: None,
        observed_account_id: Some("account-1".to_owned()),
        observed_remote_root_id: Some("root-1".to_owned()),
        observed_remote_file_id: Some("object-1".to_owned()),
        observed_parent_id: Some("parent-1".to_owned()),
        observed_path: Some("notes/a.md".to_owned()),
        observed_local_revision: Some("11".repeat(32)),
        observed_remote_revision: Some("remote-revision-1".to_owned()),
        observed_sha256: (disposition == ReplayDisposition::VerifiedAppliedExactPostState)
            .then(|| content_fingerprint("a\nB\n").sha256),
        observed_byte_length: (disposition == ReplayDisposition::VerifiedAppliedExactPostState)
            .then_some(4),
        observed_operation_marker: (disposition
            == ReplayDisposition::VerifiedAppliedExactPostState)
            .then(|| intent.operation_marker.clone()),
        forbidden_side_effect: false,
        verified_received_byte_offset: None,
        resume_reference: None,
        evidence_fingerprint: String::new(),
        captured_at_unix_ms: 2,
    };
    evidence.evidence_fingerprint = evidence.canonical_fingerprint();
    let state = MutationState {
        operation_id: intent.operation_id,
        phase: if disposition == ReplayDisposition::VerifiedAppliedExactPostState {
            MutationPhase::Completed
        } else {
            MutationPhase::NeedsReconcile
        },
        attempt_number: 1,
        state_version: 2,
        disposition: Some(mutation_disposition),
        next_attempt_at_unix_ms: None,
        retry_mode: None,
        resume_reference: None,
        last_evidence_id: Some(evidence.evidence_id),
        outcome_code: None,
        updated_at_unix_ms: 2,
    };
    ReplayProof::from_r3_1(&intent, &state, &evidence).expect("sealed R3.1 replay proof")
}

fn replay_evidence(proof: &ReplayProof) -> ClassificationEvidence {
    let mut facts = exact_facts();
    proof.bind_facts(&mut facts);
    if proof.disposition() == ReplayDisposition::VerifiedAppliedExactPostState {
        facts.local = proof.expected_local_content().cloned();
    }
    ClassificationEvidence::new(facts)
}

fn assert_result(case: ConflictCase, cell: ConflictCell, outcome: ConflictOutcome) {
    let plan = classify_conflict(input(case));
    assert_eq!(plan.cell(), cell);
    assert_eq!(plan.cell().as_str(), cell.as_str());
    assert_eq!(plan.outcome(), outcome);
}

#[test]
fn canonical_c01_through_c05_results_are_frozen() {
    assert_result(
        ConflictCase::RemoteContentChanged {
            guarded_local_replace: GuardedLocalCapability::Available,
        },
        ConflictCell::C01,
        ConflictOutcome::GuardedLocalReplace,
    );
    assert_result(
        ConflictCase::LocalContentChanged,
        ConflictCell::C02,
        ConflictOutcome::RemoteMutationBlocked,
    );
    assert_result(
        ConflictCase::NonOverlappingMarkdownChanges,
        ConflictCell::C03,
        ConflictOutcome::SafeTextMergeLocal,
    );
    assert_result(
        ConflictCase::OverlappingTextChanges,
        ConflictCell::C04,
        ConflictOutcome::PreserveBothLocal,
    );
    assert_result(
        ConflictCase::BothBinaryChanged,
        ConflictCell::C05,
        ConflictOutcome::PreserveBothLocal,
    );
}

#[test]
fn c01_fails_closed_without_the_required_local_replace_capability() {
    for capability in [
        GuardedLocalCapability::Unavailable,
        GuardedLocalCapability::Unknown,
    ] {
        let plan = classify_conflict(input(ConflictCase::RemoteContentChanged {
            guarded_local_replace: capability,
        }));
        assert_eq!(plan.cell(), ConflictCell::C01);
        assert_eq!(plan.outcome(), ConflictOutcome::NeedsReconcile);
        assert!(plan.drafts().is_empty());
        assert_eq!(plan.cursor_gate(), CursorGate::NeedsReconcile);
    }
}

#[test]
fn c06_selects_the_canonical_subcell_from_no_replace_capability() {
    assert_result(
        ConflictCase::BothChangedWithoutExactBase {
            guarded_no_replace: GuardedLocalCapability::Available,
        },
        ConflictCell::C06a,
        ConflictOutcome::PreserveBothLocal,
    );

    for capability in [
        GuardedLocalCapability::Unavailable,
        GuardedLocalCapability::Unknown,
    ] {
        assert_result(
            ConflictCase::BothChangedWithoutExactBase {
                guarded_no_replace: capability,
            },
            ConflictCell::C06b,
            ConflictOutcome::NeedsReconcile,
        );
    }
}

#[test]
fn c07_and_c08_preserve_only_with_proven_no_replace_publication() {
    let cases = [
        (
            ConflictCase::InvalidOrAmbiguousText {
                guarded_no_replace: GuardedLocalCapability::Available,
            },
            ConflictCell::C07,
        ),
        (
            ConflictCase::MergeInputExceedsBounds {
                guarded_no_replace: GuardedLocalCapability::Available,
            },
            ConflictCell::C08,
        ),
    ];
    for (case, cell) in cases {
        let plan = classify_conflict(input(case));
        assert_eq!(plan.cell(), cell);
        assert_eq!(plan.outcome(), ConflictOutcome::PreserveBothLocal);
        assert!(plan.drafts().contains(&ConflictDraft::ConflictCopyPublish));
    }

    for capability in [
        GuardedLocalCapability::Unavailable,
        GuardedLocalCapability::Unknown,
    ] {
        for case in [
            ConflictCase::InvalidOrAmbiguousText {
                guarded_no_replace: capability,
            },
            ConflictCase::MergeInputExceedsBounds {
                guarded_no_replace: capability,
            },
        ] {
            let plan = classify_conflict(input(case));
            assert_eq!(plan.outcome(), ConflictOutcome::NeedsReconcile);
            assert!(!plan.drafts().contains(&ConflictDraft::ConflictCopyPublish));
            assert_eq!(plan.cursor_gate(), CursorGate::NeedsReconcile);
        }
    }
}

#[test]
fn c09_and_c10_retain_delete_conflict_evidence_without_remote_mutation() {
    let c09 = classify_conflict(input(ConflictCase::LocalDeleteRemoteEdited));
    assert_eq!(c09.cell(), ConflictCell::C09);
    assert_eq!(c09.outcome(), ConflictOutcome::NeedsReconcile);
    assert!(c09.drafts().contains(&ConflictDraft::LocalContentPublish));
    assert!(c09.drafts().contains(&ConflictDraft::RemoteExistingBlocked));
    assert!(c09.retained().contains(&RetainedEvidence::DeleteIntent));
    assert_eq!(c09.cursor_gate(), CursorGate::NeedsReconcile);

    let c10 = classify_conflict(input(ConflictCase::LocalEditedRemoteDelete));
    assert_eq!(c10.cell(), ConflictCell::C10);
    assert_eq!(c10.outcome(), ConflictOutcome::NeedsReconcile);
    assert!(c10.retained().contains(&RetainedEvidence::LocalBytes));
    assert!(c10
        .retained()
        .contains(&RetainedEvidence::PreserveBothEvidence));
    assert_eq!(c10.cursor_gate(), CursorGate::NeedsReconcile);
}

#[test]
fn c11_is_verified_only_with_exact_online_final_state_proof() {
    let verified = classify_conflict(input(ConflictCase::BothDeleted {
        final_state: DeletedFinalStateProof::Verified,
    }));
    assert_eq!(verified.cell(), ConflictCell::C11);
    assert_eq!(verified.outcome(), ConflictOutcome::NoOpVerified);
    assert!(verified.drafts().is_empty());
    assert_eq!(verified.cursor_gate(), CursorGate::ExactVerification);

    let offline = classify_conflict(input(ConflictCase::BothDeleted {
        final_state: DeletedFinalStateProof::NotVerified,
    }));
    assert_eq!(offline.cell(), ConflictCell::C11);
    assert_eq!(offline.outcome(), ConflictOutcome::NeedsReconcile);
    assert_eq!(offline.cursor_gate(), CursorGate::NeedsReconcile);
}

#[test]
fn c12_publishes_remote_content_but_retains_and_blocks_local_rename() {
    let plan = classify_conflict(input(ConflictCase::LocalRenameRemoteEdited));
    assert_eq!(plan.cell(), ConflictCell::C12);
    assert_eq!(plan.outcome(), ConflictOutcome::RemoteMutationBlocked);
    assert!(plan.drafts().contains(&ConflictDraft::LocalContentPublish));
    assert!(plan
        .drafts()
        .contains(&ConflictDraft::RemoteExistingBlocked));
    assert!(plan
        .retained()
        .contains(&RetainedEvidence::LocalRenameIntent));
    assert_eq!(plan.cursor_gate(), CursorGate::NeedsReconcile);
}

#[test]
fn c13_optional_local_rename_requires_all_exact_guards() {
    let guarded = classify_conflict(input(ConflictCase::LocalEditedRemoteRename {
        local_rename: LocalRenameGuard::ExactIdentityLineageAndNoCollision,
    }));
    assert_eq!(guarded.cell(), ConflictCell::C13);
    assert_eq!(guarded.outcome(), ConflictOutcome::NeedsReconcile);
    assert_eq!(guarded.drafts(), vec![ConflictDraft::GuardedLocalRename]);
    assert!(guarded.retained().contains(&RetainedEvidence::LocalBytes));

    let unproven = classify_conflict(input(ConflictCase::LocalEditedRemoteRename {
        local_rename: LocalRenameGuard::NotProven,
    }));
    assert_eq!(unproven.cell(), ConflictCell::C13);
    assert_eq!(unproven.outcome(), ConflictOutcome::NeedsReconcile);
    assert!(unproven.drafts().is_empty());
}

#[test]
fn classifier_is_deterministic_and_has_no_arrival_or_time_input() {
    let case = ConflictCase::BothChangedWithoutExactBase {
        guarded_no_replace: GuardedLocalCapability::Available,
    };
    assert_eq!(
        classify_conflict(input(case)),
        classify_conflict(input(case))
    );
}

#[test]
fn approved_classifier_input_rejects_missing_identity_and_unproven_merge() {
    let mut missing_facts = exact_facts();
    missing_facts.account_id = None;
    let missing_identity = ClassificationEvidence::new(missing_facts);
    assert_eq!(
        ConflictInput::fresh(missing_identity, ConflictCase::LocalContentChanged),
        Err(ConflictInputFailure::MissingExactIdentity)
    );

    let missing_merge = ClassificationEvidence::new(exact_facts());
    assert_eq!(
        ConflictInput::fresh(missing_merge, ConflictCase::NonOverlappingMarkdownChanges,),
        Err(ConflictInputFailure::MissingMarkdownMergeProof)
    );

    let mut mismatched_facts = exact_facts();
    mismatched_facts.base = Some(content_fingerprint("different base\n"));
    assert_eq!(
        ClassificationEvidence::new(mismatched_facts)
            .verify_markdown_merge("a\nb\n", "A\nb\n", "a\nB\n"),
        Err(myvault_sync_engine::conflict::MarkdownMergeIssue::EvidenceFingerprintMismatch)
    );
}

#[test]
fn terminal_no_op_and_guarded_metadata_require_sealed_semantic_proofs() {
    let unsealed = ClassificationEvidence::new(exact_facts());
    for case in [
        ConflictCase::BothDeleted {
            final_state: DeletedFinalStateProof::Verified,
        },
        ConflictCase::SameExactRename,
        ConflictCase::SameExactMove,
    ] {
        assert_eq!(
            ConflictInput::fresh(unsealed.clone(), case),
            Err(ConflictInputFailure::MissingSemanticProof)
        );
    }

    let mut missing_target = exact_facts();
    missing_target.guarded_metadata_target = None;
    assert_eq!(
        ConflictInput::fresh(
            ClassificationEvidence::new(missing_target),
            ConflictCase::LocalEditedRemoteRename {
                local_rename: LocalRenameGuard::ExactIdentityLineageAndNoCollision,
            },
        ),
        Err(ConflictInputFailure::MissingSemanticProof)
    );

    let mut collided_target = exact_facts();
    let target = collided_target
        .guarded_metadata_target
        .as_mut()
        .expect("target");
    target
        .occupied_collision_keys
        .push(target.normalized_collision_key.clone());
    assert_eq!(
        ConflictInput::fresh(
            ClassificationEvidence::new(collided_target),
            ConflictCase::LocalEditedRemoteMove {
                local_move: LocalRenameGuard::ExactIdentityLineageAndNoCollision,
            },
        ),
        Err(ConflictInputFailure::InvalidExactIdentity)
    );
}

#[test]
fn canonical_c14_through_c21_results_are_frozen() {
    let cases = [
        (
            ConflictCase::SameExactRename,
            ConflictCell::C14a,
            ConflictOutcome::NoOpVerified,
        ),
        (
            ConflictCase::EquivalentButByteDifferentRenames,
            ConflictCell::C14b,
            ConflictOutcome::NeedsReconcile,
        ),
        (
            ConflictCase::DivergentRenames,
            ConflictCell::C14c,
            ConflictOutcome::NeedsReconcile,
        ),
        (
            ConflictCase::LocalMoveRemoteEdited,
            ConflictCell::C15,
            ConflictOutcome::NeedsReconcile,
        ),
        (
            ConflictCase::LocalEditedRemoteMove {
                local_move: LocalRenameGuard::NotProven,
            },
            ConflictCell::C16,
            ConflictOutcome::NeedsReconcile,
        ),
        (
            ConflictCase::SameExactMove,
            ConflictCell::C17,
            ConflictOutcome::NoOpVerified,
        ),
        (
            ConflictCase::DivergentMoves,
            ConflictCell::C18,
            ConflictOutcome::NeedsReconcile,
        ),
        (
            ConflictCase::RenameMoveConflict {
                combined_target: LocalRenameGuard::NotProven,
            },
            ConflictCell::C19,
            ConflictOutcome::NeedsReconcile,
        ),
        (
            ConflictCase::LocalMetadataChangeRemoteDelete,
            ConflictCell::C20,
            ConflictOutcome::NeedsReconcile,
        ),
        (
            ConflictCase::LocalDeleteRemoteMetadataChange,
            ConflictCell::C21,
            ConflictOutcome::NeedsReconcile,
        ),
    ];
    for (case, cell, outcome) in cases {
        assert_result(case, cell, outcome);
    }
}

#[test]
fn c16_c19_and_c26_emit_local_metadata_drafts_only_after_exact_guards() {
    for (case, expected) in [
        (
            ConflictCase::LocalEditedRemoteMove {
                local_move: LocalRenameGuard::ExactIdentityLineageAndNoCollision,
            },
            ConflictDraft::GuardedLocalMove,
        ),
        (
            ConflictCase::RenameMoveConflict {
                combined_target: LocalRenameGuard::ExactIdentityLineageAndNoCollision,
            },
            ConflictDraft::GuardedLocalRenameMove,
        ),
        (
            ConflictCase::ChildChangedWithParentChanged {
                resolved_lineage: LocalRenameGuard::ExactIdentityLineageAndNoCollision,
            },
            ConflictDraft::GuardedLocalMove,
        ),
    ] {
        let plan = classify_conflict(input(case));
        assert_eq!(plan.outcome(), ConflictOutcome::NeedsReconcile);
        assert_eq!(plan.drafts(), vec![expected]);
        assert_eq!(plan.cursor_gate(), CursorGate::NeedsReconcile);
    }
}

#[test]
fn rename_move_cycle_maps_to_fail_closed_c18_without_draft() {
    let plan = classify_conflict(input(ConflictCase::RenameMoveCycle));
    assert_eq!(plan.cell(), ConflictCell::C18);
    assert_eq!(plan.outcome(), ConflictOutcome::NeedsReconcile);
    assert!(plan.drafts().is_empty());
    assert!(plan.retained().contains(&RetainedEvidence::ParentLineage));
}

#[test]
fn c22_c23_and_c34_require_no_replace_for_conflict_copy_publication() {
    let cases = [
        (
            ConflictCase::DestinationCreatedVsMoveOrRename {
                guarded_no_replace: GuardedLocalCapability::Available,
            },
            ConflictCell::C22a,
        ),
        (
            ConflictCase::DistinctCreatesAtEquivalentPath {
                guarded_no_replace: GuardedLocalCapability::Available,
            },
            ConflictCell::C23,
        ),
        (
            ConflictCase::PortableEquivalentDestinationCollision {
                guarded_no_replace: GuardedLocalCapability::Available,
            },
            ConflictCell::C34,
        ),
    ];
    for (case, cell) in cases {
        assert_result(case, cell, ConflictOutcome::PreserveBothLocal);
    }

    assert_result(
        ConflictCase::DestinationCreatedVsMoveOrRename {
            guarded_no_replace: GuardedLocalCapability::Unknown,
        },
        ConflictCell::C22b,
        ConflictOutcome::NeedsReconcile,
    );
    assert_result(
        ConflictCase::DistinctCreatesAtEquivalentPath {
            guarded_no_replace: GuardedLocalCapability::Unavailable,
        },
        ConflictCell::C23,
        ConflictOutcome::NeedsReconcile,
    );
    assert_result(
        ConflictCase::PortableEquivalentDestinationCollision {
            guarded_no_replace: GuardedLocalCapability::Unavailable,
        },
        ConflictCell::C34,
        ConflictOutcome::NeedsReconcile,
    );
}

#[test]
fn c24_c25_and_c26_fail_closed_without_identity_or_lineage_inference() {
    assert_result(
        ConflictCase::DifferentItemsMoveToSameTarget,
        ConflictCell::C24,
        ConflictOutcome::NeedsReconcile,
    );
    assert_result(
        ConflictCase::DuplicateRemotePath,
        ConflictCell::C25,
        ConflictOutcome::NeedsReconcile,
    );
    let c26 = classify_conflict(input(ConflictCase::ChildChangedWithParentChanged {
        resolved_lineage: LocalRenameGuard::NotProven,
    }));
    assert_eq!(c26.cell(), ConflictCell::C26);
    assert_eq!(c26.outcome(), ConflictOutcome::NeedsReconcile);
    assert!(c26.drafts().is_empty());
}

#[test]
fn boundary_precedence_selects_c27_c28_and_c33_before_fresh_policy() {
    let base_case = ConflictCase::NonOverlappingMarkdownChanges;
    for (boundary, cell, outcome) in [
        (
            BoundaryAssessment::ProtectedPath,
            ConflictCell::C27,
            ConflictOutcome::UnsupportedProtected,
        ),
        (
            BoundaryAssessment::UnsupportedObjectOrTopology,
            ConflictCell::C28a,
            ConflictOutcome::UnsupportedProtected,
        ),
        (
            BoundaryAssessment::MalformedObjectMetadata,
            ConflictCell::C28b,
            ConflictOutcome::NeedsReconcile,
        ),
        (
            BoundaryAssessment::AccountRootOrAllowlistMismatch,
            ConflictCell::C33a,
            ConflictOutcome::UnsupportedProtected,
        ),
        (
            BoundaryAssessment::AllowlistedIdentityLineageRevisionOrBaseMismatch,
            ConflictCell::C33b,
            ConflictOutcome::NeedsReconcile,
        ),
    ] {
        let plan = classify_conflict(
            ConflictInput::new(
                exact_evidence(),
                boundary,
                ReplayAssessment::VerifiedAppliedExactPostState(replay_proof(
                    ReplayDisposition::VerifiedAppliedExactPostState,
                    MutationOperationKind::LocalPublish,
                )),
                base_case,
            )
            .expect("boundary input"),
        );
        assert_eq!(plan.cell(), cell);
        assert_eq!(plan.outcome(), outcome);
        assert!(plan.drafts().is_empty());
    }
}

#[test]
fn replay_precedence_selects_c29_through_c32_before_fresh_policy() {
    let base_case = ConflictCase::NonOverlappingMarkdownChanges;
    let applied = replay_proof(
        ReplayDisposition::VerifiedAppliedExactPostState,
        MutationOperationKind::LocalPublish,
    );
    let c29 = classify_conflict(
        ConflictInput::new(
            replay_evidence(&applied),
            BoundaryAssessment::Approved,
            ReplayAssessment::VerifiedAppliedExactPostState(applied),
            base_case,
        )
        .expect("replay input"),
    );
    assert_eq!(c29.cell(), ConflictCell::C29);
    assert_eq!(c29.outcome(), ConflictOutcome::NoOpVerified);

    for (retry, outcome) in [
        (
            VerifiedNotAppliedRetry::GuardedLocalReplace,
            ConflictOutcome::GuardedLocalReplace,
        ),
        (
            VerifiedNotAppliedRetry::GuardedConflictCopy,
            ConflictOutcome::PreserveBothLocal,
        ),
        (
            VerifiedNotAppliedRetry::RemoteExistingMutationBlocked,
            ConflictOutcome::RemoteMutationBlocked,
        ),
        (
            VerifiedNotAppliedRetry::PreconditionsChangedOrCapabilityUnavailable,
            ConflictOutcome::NeedsReconcile,
        ),
    ] {
        let operation_kind = match retry {
            VerifiedNotAppliedRetry::GuardedLocalReplace
            | VerifiedNotAppliedRetry::PreconditionsChangedOrCapabilityUnavailable => {
                MutationOperationKind::LocalPublish
            }
            VerifiedNotAppliedRetry::GuardedConflictCopy => {
                MutationOperationKind::ConflictCopyPublish
            }
            VerifiedNotAppliedRetry::RemoteExistingMutationBlocked => {
                MutationOperationKind::RemoteExistingBlocked
            }
        };
        let proof = replay_proof(ReplayDisposition::VerifiedNotApplied, operation_kind);
        let plan = classify_conflict(
            ConflictInput::new(
                replay_evidence(&proof),
                BoundaryAssessment::Approved,
                ReplayAssessment::VerifiedNotApplied { proof, retry },
                base_case,
            )
            .expect("retry input"),
        );
        assert_eq!(plan.cell(), ConflictCell::C30);
        assert_eq!(plan.outcome(), outcome);
    }

    for (proof, cell) in [
        (
            replay_proof(
                ReplayDisposition::SideEffectOutcomeUnknown,
                MutationOperationKind::LocalPublish,
            ),
            ConflictCell::C31,
        ),
        (
            replay_proof(
                ReplayDisposition::QueuedIntentCapturedBaseChanged,
                MutationOperationKind::LocalPublish,
            ),
            ConflictCell::C32,
        ),
    ] {
        let evidence = replay_evidence(&proof);
        let replay = match cell {
            ConflictCell::C31 => ReplayAssessment::SideEffectOutcomeUnknown(proof),
            ConflictCell::C32 => ReplayAssessment::QueuedIntentCapturedBaseChanged(proof),
            _ => unreachable!("replay fixture cell"),
        };
        let plan = classify_conflict(
            ConflictInput::new(evidence, BoundaryAssessment::Approved, replay, base_case)
                .expect("reconcile input"),
        );
        assert_eq!(plan.cell(), cell);
        assert_eq!(plan.outcome(), ConflictOutcome::NeedsReconcile);
        assert!(plan.drafts().is_empty());
    }
}

#[test]
fn approved_replay_requires_proof_bound_to_the_prior_operation_and_state() {
    let applied = replay_proof(
        ReplayDisposition::VerifiedAppliedExactPostState,
        MutationOperationKind::LocalPublish,
    );
    let different_intent = replay_proof(
        ReplayDisposition::VerifiedAppliedExactPostState,
        MutationOperationKind::ConflictCopyPublish,
    );
    assert_eq!(
        ConflictInput::new(
            replay_evidence(&different_intent),
            BoundaryAssessment::Approved,
            ReplayAssessment::VerifiedAppliedExactPostState(applied.clone()),
            ConflictCase::LocalContentChanged,
        ),
        Err(ConflictInputFailure::InvalidReplayProof)
    );

    for mutation in 0..4 {
        let mut facts = exact_facts();
        applied.bind_facts(&mut facts);
        match mutation {
            0 => facts.remote_root_id = Some("other-root".to_owned()),
            1 => facts.object_identity = Some("other-object".to_owned()),
            2 => facts.base = Some(content_fingerprint("changed base\n")),
            3 => facts.local_revision = Some("22".repeat(32)),
            _ => unreachable!("bounded fixture"),
        }
        let evidence = ClassificationEvidence::new(facts);
        assert_eq!(
            ConflictInput::new(
                evidence,
                BoundaryAssessment::Approved,
                ReplayAssessment::VerifiedAppliedExactPostState(applied.clone()),
                ConflictCase::LocalContentChanged,
            ),
            Err(ConflictInputFailure::InvalidReplayProof)
        );
    }

    let unknown = replay_proof(
        ReplayDisposition::SideEffectOutcomeUnknown,
        MutationOperationKind::LocalPublish,
    );
    assert_eq!(
        ConflictInput::new(
            replay_evidence(&applied),
            BoundaryAssessment::Approved,
            ReplayAssessment::SideEffectOutcomeUnknown(unknown),
            ConflictCase::LocalContentChanged,
        ),
        Err(ConflictInputFailure::InvalidReplayProof)
    );

    let verified_not_applied = replay_proof(
        ReplayDisposition::VerifiedNotApplied,
        MutationOperationKind::LocalPublish,
    );
    assert_eq!(
        ConflictInput::new(
            replay_evidence(&applied),
            BoundaryAssessment::Approved,
            ReplayAssessment::VerifiedNotApplied {
                proof: verified_not_applied,
                retry: VerifiedNotAppliedRetry::GuardedLocalReplace,
            },
            ConflictCase::LocalContentChanged,
        ),
        Err(ConflictInputFailure::InvalidReplayProof)
    );

    assert_eq!(
        ConflictInput::new(
            replay_evidence(&applied),
            BoundaryAssessment::Approved,
            ReplayAssessment::SideEffectOutcomeUnknown(applied),
            ConflictCase::LocalContentChanged,
        ),
        Err(ConflictInputFailure::InvalidReplayProof)
    );
}
