use super::*;

fn sql(store: &Store, statement: &str) {
    store.conn.lock().unwrap().execute_batch(statement).unwrap();
}
fn count(store: &Store, name: &str) -> i64 {
    store
        .conn
        .lock()
        .unwrap()
        .query_row(
            &format!("SELECT COUNT(*) FROM {}", ident(name)),
            [],
            |row| row.get(0),
        )
        .unwrap()
}
fn conversation(store: &Store, id: &str, title: &str) {
    store
        .conn
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO conversations(id,data,title) VALUES (?1,'{}',?2)",
            params![id, title],
        )
        .unwrap();
}
fn keyed(key: u8) -> Store {
    let mut store = Store::open_in_memory();
    store.master_key = Zeroizing::new(vec![key; 32]);
    store
}
#[tokio::test]
async fn imports_history_without_activating_jobs_or_replaying_notices() {
    let source = keyed(1);
    let target = keyed(2);
    conversation(&source, "legacy", "old");
    conversation(&target, "current", "new");
    sql(&source,"INSERT INTO messages VALUES ('legacy',0,'{\"text\":\"retained\"}');
        INSERT INTO scheduled_jobs(id,schedule,task,enabled,next_run_at,created_at,conversation_id) VALUES ('job','0 9 * * *','fresh task',1,'2026-10-10T09:00:00Z','2026-10-01T00:00:00Z','legacy');
        INSERT INTO job_runs(id,job_id,status,output,started_at,finished_at) VALUES ('run','job','done','old protected output','2026-10-01T00:00:00Z','2026-10-01T00:01:00Z');
        INSERT INTO work_outbox(id,parent,channel,body,created_at) VALUES ('notice','closed','telegram','historical notice','2026-10-01T00:00:00Z');
        INSERT INTO work_item_events(item,at,kind,actor) VALUES ('closed','2026-10-01T00:00:00Z','test','old');");
    sql(&target,"INSERT INTO work_item_events(item,at,kind,actor) VALUES ('current','2026-10-09T00:00:00Z','test','current');");
    let encrypted = source
        .secrets()
        .encrypt_with_aad("test_credential", b"confidential-value")
        .unwrap();
    source
        .conn
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO secrets(name,data) VALUES ('test_credential',?1)",
            [encrypted],
        )
        .unwrap();
    let report = target.import_instance(&source, "main").await.unwrap();
    assert_eq!(count(&target, "conversations"), 2);
    assert_eq!(report.previously_enabled_schedules, vec!["job"]);
    assert_eq!(report.retired_pending_notices, 1);
    assert_eq!(report.reencrypted_values, 1);
    assert!(target.work_outbox_pending().await.unwrap().is_empty());
    assert!(!target.work_outbox_mark_delivered("notice").await.unwrap());
    {
        let conn = target.conn.lock().unwrap();
        assert_eq!(
            conn.query_row("SELECT enabled FROM scheduled_jobs", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row(
                "SELECT actor FROM work_item_events ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "current"
        );
        let cipher = conn
            .query_row("SELECT data FROM secrets", [], |r| r.get::<_, Vec<u8>>(0))
            .unwrap();
        assert_eq!(
            &*target
                .secrets()
                .decrypt_with_aad("test_credential", &cipher)
                .unwrap(),
            b"confidential-value"
        );
        assert!(!serde_json::to_string(&report)
            .unwrap()
            .contains("confidential-value"));
    }
    assert_eq!(
        report,
        target.import_instance(&source, "main").await.unwrap()
    );
    assert_eq!(count(&target, "work_item_events"), 2);
    assert_eq!(count(&source, "instance_imports"), 0);
}
#[tokio::test]
async fn conflicts_and_invalid_keys_roll_back_the_entire_import() {
    let mut source = keyed(1);
    let target = keyed(2);
    conversation(&source, "fresh", "fresh");
    conversation(&source, "shared", "old");
    conversation(&target, "shared", "current");
    assert!(target
        .import_instance(&source, "main")
        .await
        .unwrap_err()
        .to_string()
        .contains("conflicting primary key"));
    assert_eq!(count(&target, "conversations"), 1);
    assert_eq!(count(&target, "instance_imports"), 0);
    sql(&source, "DELETE FROM conversations WHERE id='shared'");
    let cipher = source
        .secrets()
        .encrypt_with_aad("credential", b"secret")
        .unwrap();
    source
        .conn
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO secrets(name,data) VALUES ('credential',?1)",
            [cipher],
        )
        .unwrap();
    source.master_key = Zeroizing::new(vec![3; 32]);
    assert!(target.import_instance(&source, "main").await.is_err());
    assert_eq!(count(&target, "conversations"), 1);
    assert_eq!(count(&target, "secrets"), 0);
}
#[tokio::test]
async fn a_receipt_refuses_changed_source_state() {
    let source = keyed(1);
    let target = keyed(2);
    target.import_instance(&source, "builder").await.unwrap();
    conversation(&source, "changed", "changed");
    assert!(target
        .import_instance(&source, "builder")
        .await
        .unwrap_err()
        .to_string()
        .contains("different state"));
    assert_eq!(count(&target, "conversations"), 0);
}
#[test]
fn backup_captures_committed_wal_and_refuses_overwrite() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source.db");
    let output = tmp.path().join("snapshot.db");
    let conn = Connection::open(&source).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE evidence(id INTEGER PRIMARY KEY); INSERT INTO evidence VALUES (1);").unwrap();
    assert_eq!(
        Store::snapshot_database(&source, &output).unwrap().len(),
        64
    );
    assert!(Store::snapshot_database(&source, &output).is_err());
    assert_eq!(
        Connection::open(output)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM evidence", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn encrypted_versions_and_pending_requests_retain_their_authority() {
    let source = keyed(1);
    let target = keyed(2);
    let version = source
        .secrets()
        .encrypt_with_aad("account", b"previous")
        .unwrap();
    let proposed = source
        .secrets()
        .encrypt_with_aad("request", b"proposal")
        .unwrap();
    {
        let conn = source.conn.lock().unwrap();
        conn.execute("INSERT INTO secret_versions(name,version,data,replaced_at,replaced_by) VALUES ('account',7,?1,1,'user')", [version]).unwrap();
        conn.execute("INSERT INTO credential_requests(id,name,action,proposed_data,status,created_at,target_version) VALUES ('request','account','update',?1,'pending',1,8)", [proposed]).unwrap();
    }
    let report = target.import_instance(&source, "main").await.unwrap();
    assert_eq!(report.reencrypted_values, 2);
    let conn = target.conn.lock().unwrap();
    let value: Vec<u8> = conn
        .query_row(
            "SELECT data FROM secret_versions WHERE version=7",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        &*target
            .secrets()
            .decrypt_with_aad("account", &value)
            .unwrap(),
        b"previous"
    );
    let (value, status, version): (Vec<u8>, String, i64) = conn.query_row("SELECT proposed_data,status,target_version FROM credential_requests WHERE id='request'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    assert_eq!(
        &*target
            .secrets()
            .decrypt_with_aad("request", &value)
            .unwrap(),
        b"proposal"
    );
    assert_eq!(status, "pending");
    assert_eq!(version, 8);
}

#[tokio::test]
async fn executable_work_refuses_import_and_held_work_stays_held() {
    let source = keyed(1);
    let target = keyed(2);
    sql(&source, "INSERT INTO work_items(id,kind,title,objective,done_when,status,held_by,created_at,updated_at,budget,trigger) VALUES ('held','code','legacy','legacy','legacy','blocked','consent-plan','2026-10-01','2026-10-01','{}','{}');
    INSERT INTO workers(name,kind,created_at,updated_at) VALUES ('old-local','local','2026-10-01','2026-10-01');
    INSERT INTO routing_defaults(class,tier,set_by,set_at) VALUES ('code',0,'old-policy','2026-10-01');");
    for status in ["queued", "ready", "leased", "running", "verifying"] {
        source
            .conn
            .lock()
            .unwrap()
            .execute("UPDATE work_items SET status=?1", [status])
            .unwrap();
        assert!(target
            .import_instance(&source, "builder")
            .await
            .unwrap_err()
            .to_string()
            .contains("executable work"));
        assert_eq!(count(&target, "work_items"), 0);
    }
    sql(&source, "UPDATE work_items SET status='blocked'");
    let report = target.import_instance(&source, "builder").await.unwrap();
    assert_eq!(report.retained_in_snapshot["workers"], 1);
    assert_eq!(count(&target, "workers"), 0);
    assert_eq!(count(&target, "routing_defaults"), 0);
    let conn = target.conn.lock().unwrap();
    let state: (String, String) = conn
        .query_row("SELECT status,held_by FROM work_items", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(state, ("blocked".into(), "consent-plan".into()));
}
