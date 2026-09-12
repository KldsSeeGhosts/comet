use super::*;
use serde_json::json;
use std::sync::Barrier;
use tokio::sync::broadcast::error::TryRecvError;

fn scope() -> Scope {
    Scope {
        workspace: "workspace".into(),
        worktree: "tree".into(),
    }
}

fn spec(count: usize) -> TeamSpec {
    TeamSpec {
        scope: scope(),
        roles: (0..count)
            .map(|n| RoleSpec {
                label: format!("role-{n}"),
                provider: "pi".into(),
                prompt: "Do the assigned task".into(),
            })
            .collect(),
    }
}

fn report(summary: &str) -> Report {
    Report {
        summary: summary.into(),
        result_file: Some("results/review.txt".into()),
    }
}

fn memory() -> Store {
    Store::open(":memory:").unwrap()
}

#[test]
fn whole_team_validation_has_no_writes_or_events() {
    let store = memory();
    let mut subscription = store.subscribe(&scope()).unwrap();
    for count in [0, 9] {
        assert!(spec(count).validate().is_err());
        assert!(store.team_create(spec(count)).is_err());
    }
    let mut bad = spec(2);
    bad.roles[1].label = bad.roles[0].label.clone();
    assert!(store.team_create(bad).is_err());
    let mut bad = spec(2);
    bad.roles[1].prompt = "x".repeat(MAX_PROMPT_BYTES + 1);
    assert!(bad.validate().is_err());
    assert!(store.team_create(bad).is_err());
    let mut bad = spec(1);
    bad.scope.worktree = "\0".into();
    assert!(store.team_create(bad).is_err());
    assert!(store.team_list(&scope()).unwrap().is_empty());
    assert_eq!(subscription.events.try_recv(), Err(TryRecvError::Empty));
    let mut largest = spec(MAX_ROLES);
    for role in &mut largest.roles {
        role.prompt = "\u{1}".repeat(MAX_PROMPT_BYTES);
    }
    let team = store.team_create(largest).unwrap();
    assert_eq!(store.team_get(&scope(), &team.id).unwrap(), Some(team));
}

#[test]
fn reporting_requires_exact_identity_and_explicit_reports() {
    let store = memory();
    let team = store.team_create(spec(2)).unwrap();
    let id = &team.id;
    assert!(
        store
            .team_report(&scope(), id, "role-0", "s0", report("done"))
            .is_err()
    );
    store
        .bind_role_session(&scope(), id, "role-0", "s0")
        .unwrap();
    let bound = store
        .bind_role_session(&scope(), id, "role-0", "s0")
        .unwrap();
    assert!(
        store
            .bind_role_session(&scope(), id, "role-0", "replacement")
            .is_err()
    );
    assert!(
        store
            .bind_role_session(&scope(), id, "role-1", "s0")
            .is_err()
    );
    assert_eq!(store.team_get(&scope(), id).unwrap(), Some(bound));
    store
        .bind_role_session(&scope(), id, "role-1", "s1")
        .unwrap();
    let other_tree = Scope {
        worktree: "different".into(),
        ..scope()
    };
    let other_workspace = Scope {
        workspace: "different".into(),
        ..scope()
    };
    for wrong_scope in [other_tree, other_workspace] {
        assert!(store.team_get(&wrong_scope, id).unwrap().is_none());
        assert!(store.team_list(&wrong_scope).unwrap().is_empty());
        assert!(
            store
                .team_report(&wrong_scope, id, "role-0", "s0", report("done"))
                .is_err()
        );
        assert!(
            store
                .bind_role_session(&wrong_scope, id, "role-0", "s0")
                .is_err()
        );
        assert!(store.team_cancel(&wrong_scope, id).is_err());
    }
    assert!(
        store
            .team_report(&scope(), id, "role-1", "s0", report("done"))
            .is_err()
    );
    assert!(
        store
            .team_report(&scope(), id, "missing", "s0", report("done"))
            .is_err()
    );
    let half = store
        .team_report(&scope(), id, "role-0", "s0", report("done"))
        .unwrap();
    assert_eq!(half.status, TeamStatus::Running);
    let mut sub = store.subscribe(&scope()).unwrap();
    assert_eq!(
        store
            .team_report(&scope(), id, "role-0", "s0", report("done"))
            .unwrap(),
        half
    );
    assert!(
        store
            .team_report(&scope(), id, "role-0", "s0", report("changed"))
            .is_err()
    );
    assert_eq!(sub.events.try_recv(), Err(TryRecvError::Empty));
    let complete = store
        .team_report(&scope(), id, "role-1", "s1", report("done"))
        .unwrap();
    assert_eq!(complete.status, TeamStatus::Completed);
    assert_eq!(
        store
            .team_report(&scope(), id, "role-1", "s1", report("done"))
            .unwrap(),
        complete
    );
    assert!(store.team_cancel(&scope(), id).is_err());
    assert!(
        store
            .bind_role_session(&scope(), id, "role-0", "s0")
            .is_err()
    );
}

#[test]
fn summary_limit_counts_utf8_bytes_and_preserves_retry() {
    let store = memory();
    let team = store.team_create(spec(1)).unwrap();
    store
        .bind_role_session(&scope(), &team.id, "role-0", "session")
        .unwrap();
    let exact = "é".repeat(MAX_SUMMARY_BYTES / 2);
    let mut sub = store.subscribe(&scope()).unwrap();
    assert!(
        store
            .team_report(
                &scope(),
                &team.id,
                "role-0",
                "session",
                report(&(exact.clone() + "a"))
            )
            .is_err()
    );
    let mut bad = report("fine");
    bad.result_file = Some("x".repeat(4097));
    assert!(
        store
            .team_report(&scope(), &team.id, "role-0", "session", bad)
            .is_err()
    );
    assert!(
        store.team_get(&scope(), &team.id).unwrap().unwrap().roles[0]
            .report
            .is_none()
    );
    assert_eq!(sub.events.try_recv(), Err(TryRecvError::Empty));
    let done = store
        .team_report(&scope(), &team.id, "role-0", "session", report(&exact))
        .unwrap();
    assert_eq!(done.status, TeamStatus::Completed);
    assert_eq!(
        done.roles[0].report.as_ref().unwrap().summary.len(),
        MAX_SUMMARY_BYTES
    );
}

#[test]
fn restart_recovery_is_explicit_durable_and_terminal() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    let store = Store::open(&path).unwrap();
    let running = store.team_create(spec(2)).unwrap();
    store
        .bind_role_session(&scope(), &running.id, "role-0", "s0")
        .unwrap();
    store
        .team_report(&scope(), &running.id, "role-0", "s0", report("partial"))
        .unwrap();
    let cancelled = store.team_create(spec(1)).unwrap();
    store
        .bind_role_session(&scope(), &cancelled.id, "role-0", "c0")
        .unwrap();
    let terminal = store.team_cancel(&scope(), &cancelled.id).unwrap();
    assert_eq!(
        store.team_cancel(&scope(), &cancelled.id).unwrap(),
        terminal
    );
    assert!(
        store
            .team_report(&scope(), &cancelled.id, "role-0", "c0", report("late"))
            .is_err()
    );
    let done = store.team_create(spec(1)).unwrap();
    store
        .bind_role_session(&scope(), &done.id, "role-0", "d0")
        .unwrap();
    store
        .team_report(&scope(), &done.id, "role-0", "d0", report("done"))
        .unwrap();
    let mut other_spec = spec(1);
    other_spec.scope.worktree = "other".into();
    let other = store.team_create(other_spec).unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store
            .team_get(&scope(), &running.id)
            .unwrap()
            .unwrap()
            .status,
        TeamStatus::Running
    );
    let mut sub = store.subscribe(&scope()).unwrap();
    let mut other_sub = store.subscribe(&other.scope).unwrap();
    assert_eq!(store.recover_interrupted().unwrap().len(), 2);
    let restored = store.team_get(&scope(), &running.id).unwrap().unwrap();
    assert_eq!(restored.status, TeamStatus::Interrupted);
    assert_eq!(restored.roles[0].report, Some(report("partial")));
    assert_eq!(sub.events.try_recv().unwrap(), Event::TeamChanged(restored));
    assert_eq!(sub.events.try_recv(), Err(TryRecvError::Empty));
    assert!(
        matches!(other_sub.events.try_recv().unwrap(), Event::TeamChanged(t) if t.id == other.id)
    );
    assert!(store.recover_interrupted().unwrap().is_empty());
    assert!(
        store
            .bind_role_session(&scope(), &running.id, "role-1", "s1")
            .is_err()
    );
    assert!(store.team_cancel(&scope(), &running.id).is_err());
    assert!(
        store
            .team_report(&scope(), &running.id, "role-0", "s0", report("partial"))
            .is_err()
    );
    assert_eq!(
        store
            .team_get(&scope(), &cancelled.id)
            .unwrap()
            .unwrap()
            .status,
        TeamStatus::Cancelled
    );
    assert_eq!(
        store
            .team_report(&scope(), &done.id, "role-0", "d0", report("done"))
            .unwrap()
            .status,
        TeamStatus::Completed
    );
    drop(store);
    assert_eq!(
        Store::open(&path)
            .unwrap()
            .team_get(&scope(), &running.id)
            .unwrap()
            .unwrap()
            .status,
        TeamStatus::Interrupted
    );
}

#[test]
fn cas_deletion_prevents_aba_across_restart_and_scopes() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    let store = Store::open(&path).unwrap();
    assert_eq!(store.coordination_get(&scope(), "key").unwrap().version, 0);
    let first = store
        .coordination_set(&scope(), "key", 0, json!({"a": 1}))
        .unwrap();
    assert_eq!(first.version, 1);
    assert!(
        store
            .coordination_set(&scope(), "key", 0, json!(2))
            .is_err()
    );
    assert!(store.coordination_delete(&scope(), "key", 0).is_err());
    assert_eq!(store.coordination_get(&scope(), "key").unwrap(), first);
    let tombstone = store.coordination_delete(&scope(), "key", 1).unwrap();
    assert_eq!(tombstone.version, 2);
    assert_eq!(tombstone.value, None);
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(store.coordination_get(&scope(), "key").unwrap(), tombstone);
    assert!(
        store
            .coordination_set(&scope(), "key", 0, json!(1))
            .is_err()
    );
    assert!(
        store
            .coordination_set(&scope(), "key", 1, json!(1))
            .is_err()
    );
    let third = store
        .coordination_set(&scope(), "key", 2, json!(null))
        .unwrap();
    assert_eq!(third.version, 3);
    assert_eq!(third.value, Some(json!(null)));
    for entry in [&third, &tombstone] {
        let encoded = serde_json::to_value(entry).unwrap();
        assert_eq!(encoded.get("value").is_some(), entry.value.is_some());
        assert_eq!(
            serde_json::from_value::<CoordinationEntry>(encoded).unwrap(),
            *entry
        );
    }
    assert_eq!(
        store
            .coordination_delete(&scope(), "absent", 0)
            .unwrap()
            .version,
        1
    );
    assert_eq!(
        store
            .coordination_delete(&scope(), "absent", 1)
            .unwrap()
            .version,
        2
    );
    let other_tree = Scope {
        worktree: "other".into(),
        ..scope()
    };
    let other_workspace = Scope {
        workspace: "other".into(),
        ..scope()
    };
    for other in [other_tree, other_workspace] {
        assert_eq!(store.coordination_get(&other, "key").unwrap().version, 0);
        assert_eq!(
            store
                .coordination_set(&other, "key", 0, json!("isolated"))
                .unwrap()
                .version,
            1
        );
    }
    assert_eq!(store.coordination_get(&scope(), "key").unwrap(), third);
}

#[test]
fn independent_connections_serialize_cas_race() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Store>();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    let first = Store::open(&path).unwrap();
    let second = Store::open(&path).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let joins: Vec<_> = [first, second]
        .into_iter()
        .enumerate()
        .map(|(n, store)| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store.coordination_set(&scope(), "race", 0, json!(n))
            })
        })
        .collect();
    let results: Vec<_> = joins.into_iter().map(|j| j.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        Store::open(&path)
            .unwrap()
            .coordination_get(&scope(), "race")
            .unwrap()
            .version,
        1
    );
}

#[test]
fn snapshot_events_are_scoped_committed_and_resubscribable() {
    let store = memory();
    let first = store
        .coordination_set(&scope(), "key", 0, json!(1))
        .unwrap();
    let mut sub = store.subscribe(&scope()).unwrap();
    assert_eq!(sub.snapshot.coordination, vec![first]);
    let other = Scope {
        worktree: "other".into(),
        ..scope()
    };
    let mut other_sub = store.subscribe(&other).unwrap();
    assert!(
        store
            .coordination_set(&scope(), "key", 0, json!(2))
            .is_err()
    );
    assert_eq!(sub.events.try_recv(), Err(TryRecvError::Empty));
    let next = store
        .clone()
        .coordination_delete(&scope(), "key", 1)
        .unwrap();
    assert_eq!(
        sub.events.try_recv().unwrap(),
        Event::CoordinationChanged(next.clone())
    );
    assert_eq!(store.coordination_get(&scope(), "key").unwrap(), next);
    assert_eq!(other_sub.events.try_recv(), Err(TryRecvError::Empty));
    for version in 2..260 {
        store
            .coordination_set(&scope(), "key", version, json!(version))
            .unwrap();
    }
    assert!(matches!(
        sub.events.try_recv(),
        Err(TryRecvError::Lagged(_))
    ));
    let fresh = store.subscribe(&scope()).unwrap();
    assert_eq!(fresh.snapshot.coordination[0].version, 260);
    assert_eq!(other_sub.events.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn failed_sql_transactions_roll_back_and_emit_nothing() {
    let store = memory();
    let one = store.team_create(spec(1)).unwrap();
    let two = store.team_create(spec(1)).unwrap();
    let mut sub = store.subscribe(&scope()).unwrap();
    store.lock().unwrap().db.execute_batch("CREATE TRIGGER reject_recovery BEFORE UPDATE ON teams WHEN (SELECT count(*) FROM teams WHERE json_extract(data,'$.status')='interrupted') > 0 BEGIN SELECT RAISE(ABORT,'injected failure'); END;").unwrap();
    assert!(store.recover_interrupted().is_err());
    for team in [one, two] {
        assert_eq!(
            store.team_get(&scope(), &team.id).unwrap().unwrap().status,
            TeamStatus::Running
        );
    }
    assert_eq!(sub.events.try_recv(), Err(TryRecvError::Empty));
    store.lock().unwrap().db.execute_batch("CREATE TRIGGER reject_kv BEFORE INSERT ON coordination BEGIN SELECT RAISE(ABORT,'injected failure'); END;").unwrap();
    assert!(
        store
            .coordination_set(&scope(), "key", 0, json!(1))
            .is_err()
    );
    assert_eq!(store.coordination_get(&scope(), "key").unwrap().version, 0);
    assert_eq!(sub.events.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn rejects_foreign_changed_or_future_schemas_without_modifying_them() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    let db = Connection::open(&path).unwrap();
    db.execute_batch("CREATE TABLE unrelated(secret TEXT); INSERT INTO unrelated VALUES ('keep');")
        .unwrap();
    assert!(Store::open(&path).is_err());
    assert_eq!(
        db.query_row("SELECT secret FROM unrelated", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "keep"
    );
    drop(db);
    let path = temp.path().join("owned.sqlite");
    drop(Store::open(&path).unwrap());
    let db = Connection::open(&path).unwrap();
    db.execute_batch("PRAGMA user_version=2;").unwrap();
    assert!(Store::open(&path).is_err());
    db.execute_batch("PRAGMA user_version=1; CREATE TRIGGER unexpected AFTER INSERT ON teams BEGIN DELETE FROM coordination; END;").unwrap();
    assert!(Store::open(&path).is_err());
    db.execute_batch("DROP TRIGGER unexpected; ALTER TABLE teams ADD COLUMN extra TEXT;")
        .unwrap();
    assert!(Store::open(&path).is_err());
    assert!(
        serde_json::from_value::<TeamSpec>(
            json!({"scope":scope(),"roles":spec(1).roles,"unexpected":true})
        )
        .is_err()
    );
}

#[test]
fn validates_values_and_detects_version_exhaustion_without_writes() {
    let store = memory();
    assert!(
        store
            .coordination_set(&scope(), "key", -1, json!(1))
            .is_err()
    );
    assert!(
        store
            .coordination_set(&scope(), "key", 0, json!("a".repeat(MAX_VALUE_BYTES)))
            .is_err()
    );
    let mut nested = json!(0);
    for _ in 0..33 {
        nested = json!([nested]);
    }
    assert!(store.coordination_set(&scope(), "key", 0, nested).is_err());
    assert_eq!(store.coordination_get(&scope(), "key").unwrap().version, 0);
    store
        .lock()
        .unwrap()
        .db
        .execute(
            "INSERT INTO coordination VALUES (?1,?2,?3,?4,NULL)",
            params![scope().workspace, scope().worktree, "max", i64::MAX],
        )
        .unwrap();
    assert!(
        store
            .coordination_delete(&scope(), "max", i64::MAX)
            .is_err()
    );
    assert_eq!(
        store.coordination_get(&scope(), "max").unwrap().version,
        i64::MAX
    );
}

#[test]
fn recovery_skips_undecodable_rows_and_recovers_the_rest() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    let store = Store::open(&path).unwrap();
    let good = store.team_create(spec(1)).unwrap();
    let corrupt = store.team_create(spec(1)).unwrap();
    let mismatched = store.team_create(spec(1)).unwrap();
    let mut other_tree = spec(1);
    other_tree.scope.worktree = "elsewhere".into();
    let planted = store.team_create(other_tree).unwrap();
    {
        let inner = store.lock().unwrap();
        inner
            .db
            // Valid JSON that decodes to no team, surviving the json_valid guard.
            .execute("UPDATE teams SET data='null' WHERE id=?1", params![corrupt.id])
            .unwrap();
        inner
            .db
            .execute(
                "UPDATE teams SET data=?1 WHERE id=?2",
                params![serde_json::to_string(&good).unwrap(), mismatched.id],
            )
            .unwrap();
        inner
            .db
            .execute(
                "UPDATE teams SET id='renamed' WHERE id=?1",
                params![planted.id],
            )
            .unwrap();
    }
    let recovered = store.recover_interrupted().unwrap();
    assert_eq!(recovered, vec![{
        let mut team = good.clone();
        team.status = TeamStatus::Interrupted;
        team
    }]);
    {
        let inner = store.lock().unwrap();
        assert_eq!(
            inner
                .db
                .query_row(
                    "SELECT data FROM teams WHERE id=?1",
                    params![corrupt.id],
                    |r| r.get::<_, String>(0),
                )
                .unwrap(),
            "null",
            "the skipped row keeps its stored bytes"
        );
        for (id, status) in [(&mismatched.id, "running"), (&String::from("renamed"), "running")] {
            assert_eq!(
                inner
                    .db
                    .query_row(
                        "SELECT json_extract(data,'$.status') FROM teams WHERE id=?1",
                        params![id],
                        |r| r.get::<_, String>(0),
                    )
                    .unwrap(),
                status
            );
        }
    }
    assert!(store.recover_interrupted().unwrap().is_empty());
}

#[test]
fn store_directory_tracks_the_database_location() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("state.sqlite")).unwrap();
    assert_eq!(store.dir(), temp.path());
    assert_eq!(memory().dir(), Path::new(""));
}
