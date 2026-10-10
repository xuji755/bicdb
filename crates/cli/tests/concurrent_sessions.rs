//! Concurrency acceptance for independent catalog handles sharing one engine.

use std::path::{Path, PathBuf};

use bicdb_cli::boot::{create_instance, Instance};
use bicdb_cli::config::InstanceParams;
use bicdb_sql::session::{QueryResult, Session};

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "bicdb-concurrent-sessions-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&path);
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn params(path: &Path) -> InstanceParams {
    InstanceParams::for_init(
        path,
        None,
        &[("auth.pbkdf2_iterations".into(), "1000".into())],
    )
    .unwrap()
}

fn execute(instance: &mut Instance, sql: &str) -> Vec<QueryResult> {
    let seq = instance.seq();
    Session::new(instance.pool, instance.engine, &mut instance.catalog, seq)
        .execute(sql)
        .unwrap()
}

#[test]
fn independent_catalog_handles_execute_concurrently_on_one_workspace_engine() {
    let holder = TempDir::new();
    let mut instance = create_instance(&params(&holder.0), None).unwrap();
    execute(
        &mut instance,
        "CREATE TABLE concurrent_rows (id NUMBER NOT NULL, worker NUMBER NOT NULL)",
    );
    execute(
        &mut instance,
        "CREATE UNIQUE INDEX concurrent_rows_pk ON concurrent_rows(id)",
    );

    let engine = instance.engine;
    let pool = instance.pool;
    std::thread::scope(|scope| {
        for worker in 0..4_u64 {
            let mut catalog = instance.open_worker_catalog().unwrap();
            scope.spawn(move || {
                for offset in 0..20_u64 {
                    let id = worker * 100 + offset;
                    let seq = engine.current_seq();
                    Session::new(pool, engine, &mut catalog, seq)
                        .execute(&format!(
                            "INSERT INTO concurrent_rows VALUES ({id},{worker})"
                        ))
                        .unwrap();
                }
            });
        }
    });

    let results = execute(
        &mut instance,
        "SELECT id, worker FROM concurrent_rows ORDER BY id",
    );
    let Some(QueryResult::Rows { rows, .. }) = results.last() else {
        panic!("expected rows");
    };
    assert_eq!(rows.len(), 80);
    instance.shutdown().unwrap();
}
