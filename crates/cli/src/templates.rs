//! Immutable schema and explicit logical graph-data initialization templates.
//! Generic business-row snapshots and reflink cloning remain separate capabilities.

use bicdb_catalog::dict::ColTypeCode;
use bicdb_sql::session::{GraphSnapshot, QueryResult, Session};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const FORMAT: &str = "BICDB_SCHEMA_TEMPLATE_V1";
const MAX_SCRIPT: u64 = 1024 * 1024;
const GRAPH_FORMAT: &str = "BICDB_GRAPH_TEMPLATE_V2";
const MAX_GRAPH_BYTES: u64 = 100 * 1024 * 1024;
#[derive(Debug)]
pub struct Template {
    pub(crate) format: &'static str,
    pub(crate) name: String,
    pub(crate) digest: String,
    script: String,
    graphs: Vec<GraphSnapshot>,
}
impl Template {
    /// Validated manifest format.
    pub fn format(&self) -> &str {
        self.format
    }
    /// Number of logical graph files loaded and validated.
    pub fn graph_count(&self) -> usize {
        self.graphs.len()
    }
    /// Parse locations, rather than splitting semicolons inside quoted identifiers.
    pub fn apply(&self, session: &mut Session<'_, '_, '_, '_>) -> Result<(), String> {
        if self.format == FORMAT {
            session.execute(&self.script).map_err(|e| e.to_string())?;
            return Ok(());
        }
        let statements = bicdb_sql::parser::parse_many(&self.script).map_err(|e| e.to_string())?;
        let mut base = String::new();
        let mut derived = String::new();
        for (i, statement) in statements.iter().enumerate() {
            let start = statement.location().start;
            let end = statements
                .get(i + 1)
                .map_or(self.script.len(), |next| next.location().start);
            let sql = self
                .script
                .get(start..end)
                .ok_or("invalid template statement range")?;
            let output = if matches!(
                statement,
                bicdb_sql::ast::Stmt::CreateTable(_) | bicdb_sql::ast::Stmt::CreateGraph(_)
            ) {
                &mut base
            } else {
                &mut derived
            };
            output.push_str(sql);
            output.push('\n');
        }
        session.execute(&base).map_err(|e| e.to_string())?;
        for graph in &self.graphs {
            session
                .restore_graph_snapshot(graph)
                .map_err(|e| e.to_string())?;
        }
        if !derived.trim().is_empty() {
            session.execute(&derived).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}
fn validate_snapshots(script: &str, graphs: &[GraphSnapshot]) -> Result<(), String> {
    use std::collections::BTreeSet;
    let declared = bicdb_sql::parser::parse_many(script)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter_map(|s| match s {
            bicdb_sql::ast::Stmt::CreateGraph(g) => Some(g.graph.relname),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let actual = graphs
        .iter()
        .map(|g| g.name.clone())
        .collect::<BTreeSet<_>>();
    if declared != actual || actual.len() != graphs.len() {
        return Err("graph snapshot names must match all defined graphs exactly once".into());
    }
    let mut total = 0u64;
    for graph in graphs {
        total = total.saturating_add(graph.data.len() as u64);
        if total > MAX_GRAPH_BYTES {
            return Err("graph template snapshots exceed shared 100 MiB budget".into());
        }
        Session::validate_graph_snapshot(&graph.data).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn directory(home: &Path, name: &str) -> Result<PathBuf, String> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err("初始化模板名只允许字母、数字和下划线，最多 128 字节".into());
    }
    let parent = home.join("templates");
    if parent.exists()
        && fs::symlink_metadata(&parent)
            .map_err(|error| error.to_string())?
            .file_type()
            .is_symlink()
    {
        return Err("模板目录不能是符号链接".into());
    }
    Ok(parent.join(name))
}

fn read(path: &Path, max: u64) -> Result<String, String> {
    // OsFileIo opens without following symlinks and verifies the file handle.
    use bicdb_workspace::io::{FileIo, OpenOptions as IoOptions, OsFileIo};
    let io = OsFileIo::new();
    let handle = io
        .open(path, IoOptions::new().read(true))
        .map_err(|error| error.to_string())?;
    let result = (|| {
        let size = io.size(handle).map_err(|error| error.to_string())?;
        if size > max {
            return Err("模板文件过大".into());
        }
        let mut bytes = vec![0; size as usize];
        let mut offset = 0;
        while offset < bytes.len() {
            let count = io
                .read_at(handle, &mut bytes[offset..], offset as u64)
                .map_err(|error| error.to_string())?;
            if count == 0 {
                return Err("模板文件短读".into());
            }
            offset += count;
        }
        String::from_utf8(bytes).map_err(|_| "模板不是 UTF-8".into())
    })();
    let closed = io.close(handle).map_err(|error| error.to_string());
    closed?;
    result
}

fn sha(script: &str) -> String {
    bicdb_common::sha256::digest(script.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| error.to_string())?;
    file.write_all(text.as_bytes())
        .map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Read and validate a template before creating any target workspace files.
pub fn load(home: &Path, name: &str) -> Result<Template, String> {
    let root = directory(home, name)?;
    if fs::symlink_metadata(&root)
        .map_err(|e| e.to_string())?
        .file_type()
        .is_symlink()
    {
        return Err("模板不能是符号链接".into());
    }
    let manifest = read(&root.join("manifest"), MAX_SCRIPT)?;
    let script = read(&root.join("schema.sql"), MAX_SCRIPT)?;
    validate_schema(&script)?;
    let fields: Vec<_> = manifest.lines().collect();
    if fields.first() == Some(&FORMAT) {
        if manifest.len() > 4096
            || fields.len() != 3
            || fields[1] != name
            || fields[2] != sha(&script)
        {
            return Err("初始化模板格式或 SHA-256 校验失败".into());
        }
        return Ok(Template {
            format: FORMAT,
            name: name.to_owned(),
            digest: fields[2].to_owned(),
            script,
            graphs: Vec::new(),
        });
    }
    let json: serde_json::Value =
        serde_json::from_str(&manifest).map_err(|_| "graph template manifest is not valid JSON")?;
    let object = json
        .as_object()
        .ok_or("graph template manifest must be an object")?;
    if object.len() != 4
        || json["format"] != GRAPH_FORMAT
        || json["name"] != name
        || json["schema_sha256"] != sha(&script)
    {
        return Err("graph template manifest/schema checksum mismatch".into());
    }
    let entries = json["graphs"]
        .as_array()
        .ok_or("graph template manifest lacks graph entries")?;
    if entries.len() > 512 {
        return Err("too many graph snapshots in template".into());
    }
    let mut graphs = Vec::new();
    let mut remaining = MAX_GRAPH_BYTES;
    for (i, entry) in entries.iter().enumerate() {
        if entry.as_object().map_or(true, |v| v.len() != 3) {
            return Err("invalid graph snapshot manifest entry".into());
        }
        let name = entry["name"]
            .as_str()
            .ok_or("invalid graph snapshot name")?;
        let bytes = entry["bytes"]
            .as_u64()
            .ok_or("invalid graph snapshot byte count")?;
        if bytes == 0 || bytes > remaining {
            return Err("graph template snapshots exceed shared 100 MiB budget".into());
        }
        let data = read(&root.join(format!("graph-{i}.json")), bytes)?;
        if data.len() as u64 != bytes || entry["sha256"] != sha(&data) {
            return Err("graph snapshot size or checksum mismatch".into());
        }
        remaining -= bytes;
        graphs.push(GraphSnapshot {
            name: name.to_owned(),
            data: data.into_bytes(),
        });
    }
    validate_snapshots(&script, &graphs)?;
    Ok(Template {
        format: GRAPH_FORMAT,
        name: name.to_owned(),
        digest: sha(&manifest),
        script,
        graphs,
    })
}

/// Creation-only whitelist with namespace/target ordering checks. A matching
/// checksum does not authorize DML, general ALTER, DROP, Cypher or external targets.
fn validate_schema(script: &str) -> Result<(), String> {
    use bicdb_sql::ast::{GraphIndexAction, IndexTargetKind, Stmt};
    use std::collections::{BTreeMap, BTreeSet};
    if script.len() as u64 > MAX_SCRIPT {
        return Err("初始化模板结构超过 1 MiB".into());
    }
    let statements = bicdb_sql::parser::parse_many(script).map_err(|error| error.to_string())?;
    if statements.is_empty() || statements.len() > 512 {
        return Err("初始化模板要求 1..512 条结构语句".into());
    }
    let mut objects = BTreeMap::<String, bool>::new(); // true = graph
    let mut indexes = BTreeSet::new();
    let mut fulltext = BTreeMap::new();
    for statement in statements {
        match statement {
            Stmt::CreateTable(table) => {
                bicdb_sql::bind::check_new_object_name(&table.relation.relname)
                    .map_err(|e| e.to_string())?;
                if objects.insert(table.relation.relname, false).is_some() {
                    return Err("初始化模板对象名重复".into());
                }
            }
            Stmt::CreateGraph(graph) => {
                bicdb_sql::bind::check_new_object_name(&graph.graph.relname)
                    .map_err(|e| e.to_string())?;
                if objects.insert(graph.graph.relname, true).is_some() {
                    return Err("初始化模板对象名重复".into());
                }
            }
            Stmt::Index(index) => {
                bicdb_sql::bind::check_new_object_name(&index.idxname)
                    .map_err(|e| e.to_string())?;
                if index.target_kind != IndexTargetKind::Table
                    || objects.get(&index.relation.relname) != Some(&false)
                    || !indexes.insert(index.idxname)
                {
                    return Err("初始化模板关系索引目标必须是此前定义的表，且索引名不能重复".into());
                }
            }
            Stmt::GraphIndex(index) => {
                if objects.get(&index.graph) != Some(&true) {
                    return Err("初始化模板图索引必须引用此前定义的图".into());
                }
                let name = index.name.ok_or("初始化模板图索引缺少名字")?;
                bicdb_sql::bind::check_new_object_name(&name).map_err(|e| e.to_string())?;
                match index.action {
                    GraphIndexAction::Create { .. } => {
                        if !indexes.insert(name) {
                            return Err("初始化模板索引名重复".into());
                        }
                    }
                    GraphIndexAction::FulltextCreate { .. } => {
                        if !indexes.insert(name.clone()) {
                            return Err("初始化模板索引名重复".into());
                        }
                        fulltext.insert(name, index.graph);
                    }
                    GraphIndexAction::FulltextPause => {
                        if fulltext.get(&name) != Some(&index.graph) {
                            return Err(
                                "初始化模板 PAUSE 只能用于本模板刚创建的该图全文索引".into()
                            );
                        }
                    }
                    _ => return Err("初始化模板仅允许图索引创建与新全文索引的 PAUSE".into()),
                }
            }
            _ => {
                return Err("初始化模板仅允许 CREATE TABLE / CREATE INDEX / CREATE GRAPH / 图索引创建及全文 PAUSE".into());
            }
        }
    }
    Ok(())
}

/// Stamp copied workspaces so application initialization can bind their own IDs.
pub fn write_stamp(root: &Path, format: &str, name: &str, digest: &str) -> Result<(), String> {
    write(
        &root.join("workspace-template.info"),
        &format!("{format}\n{name}\n{digest}\n"),
    )
}

/// Snapshot an idle, empty workspace's business schema, without copying rows.
pub fn publish(home: &Path, source: &Path, name: &str) -> Result<(), String> {
    publish_mode(home, source, name, false)
}
pub fn publish_graph_data(home: &Path, source: &Path, name: &str) -> Result<(), String> {
    publish_mode(home, source, name, true)
}
fn publish_mode(home: &Path, source: &Path, name: &str, graph_data: bool) -> Result<(), String> {
    let target = directory(home, name)?;
    if target.exists() {
        return Err(format!("模板 `{name}` 已存在，不能覆盖"));
    }
    let (params, _) = crate::boot::instance_params(Some(&source.join("bicdb.ini")), &[])
        .map_err(|error| error.to_string())?;
    let mut instance = crate::boot::open_instance(&params).map_err(|error| error.to_string())?;
    let exported = export(&mut instance, graph_data);
    instance.shutdown().map_err(|error| error.to_string())?;
    let (script, graphs) = exported?;
    if script.len() as u64 > MAX_SCRIPT {
        return Err("模板结构超过 1 MiB".into());
    }
    let parent = target.parent().ok_or("模板没有父目录")?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
        .map_err(|error| error.to_string())?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let staging = parent.join(format!(".{name}-{}-{stamp}", std::process::id()));
    fs::create_dir(&staging).map_err(|error| error.to_string())?;
    let result = (|| {
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
        write(&staging.join("schema.sql"), &script)?;
        let manifest = if graph_data {
            let mut entries = Vec::new();
            for (i, graph) in graphs.iter().enumerate() {
                let data =
                    std::str::from_utf8(&graph.data).map_err(|_| "graph snapshot is not UTF-8")?;
                write(&staging.join(format!("graph-{i}.json")), data)?;
                entries.push(
                    serde_json::json!({"name":graph.name,"bytes":data.len(),"sha256":sha(data)}),
                );
            }
            serde_json::json!({"format":GRAPH_FORMAT,"name":name,"schema_sha256":sha(&script),"graphs":entries}).to_string()+"\n"
        } else {
            format!("{FORMAT}\n{name}\n{}\n", sha(&script))
        };
        if manifest.len() as u64 > MAX_SCRIPT {
            return Err("template manifest exceeds 1 MiB".into());
        }
        write(&staging.join("manifest"), &manifest)?;
        fs::File::open(&staging)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        fs::rename(&staging, &target).map_err(|error| error.to_string())?;
        fs::File::open(parent)
            .and_then(|file| file.sync_all())
            .map_err(|error| error.to_string())?;
        Ok(())
    })();
    if staging.exists() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn export(
    instance: &mut crate::boot::Instance,
    graph_data: bool,
) -> Result<(String, Vec<GraphSnapshot>), String> {
    let snapshot =
        bicdb_common::seq::CommitSeq::from_raw(instance.seq() + 1).ok_or("提交序号越界")?;
    let tables = instance
        .catalog
        .tables()
        .map_err(|error| error.to_string())?;
    let mut script = String::new();
    for table in tables
        .into_iter()
        .filter(|table| table.obj >= bicdb_catalog::dict::obj_kind::USER_FIRST)
    {
        let sequence = instance.seq();
        let mut session = Session::new(
            instance.pool,
            instance.engine,
            &mut instance.catalog,
            sequence,
        );
        let rows = session
            .execute(&format!("SELECT COUNT(*) FROM {}", quote(&table.name)))
            .map_err(|error| error.to_string())?;
        if !matches!(rows.last(), Some(QueryResult::Rows { rows, .. }) if rows.len() == 1 && rows[0][0] == bicdb_exec::Value::Number(bicdb_types::Number::parse("0").expect("zero")))
        {
            return Err(format!("初始化模板不允许业务数据：`{}` 非空", table.name));
        }
        drop(session);
        let options = instance
            .catalog
            .table_options(snapshot, table.obj)
            .map_err(|error| error.to_string())?;
        if !options.transactional
            || options.logging != 0
            || options.update_mode != 0
            || options.retention != 0
            || options.version_keep != 0
            || options.cleanup != 0
            || options.embed != 0
            || options.shared
        {
            return Err(format!("初始化模板尚不支持 `{}` 的特殊表选项", table.name));
        }
        let columns = instance
            .catalog
            .columns(snapshot, table.obj)
            .map_err(|error| error.to_string())?;
        let mut definitions = Vec::new();
        for column in &columns {
            if column.precision.is_some() || column.scale.is_some() {
                return Err("初始化模板暂不支持精度或小数位定义".into());
            }
            let kind = match ColTypeCode::from_u8(column.type_code as u8) {
                Some(ColTypeCode::Number) => "NUMBER".to_owned(),
                Some(ColTypeCode::Boolean) => "BOOLEAN".to_owned(),
                Some(ColTypeCode::Varchar2) => format!("VARCHAR2({})", column.length),
                Some(ColTypeCode::Bytes) => format!("BYTES({})", column.length),
                _ => return Err(format!("初始化模板暂不支持 `{}` 的该列类型", table.name)),
            };
            definitions.push(format!(
                "{} {kind}{}",
                quote(&column.name),
                if column.nullable { "" } else { " NOT NULL" }
            ));
        }
        script.push_str(&format!(
            "CREATE TABLE {} ({}) WITH (pctfree={}, itl_max={});\n",
            quote(&table.name),
            definitions.join(", "),
            options.pctfree,
            options.itl_max
        ));
        for index in instance
            .catalog
            .indexes_of(snapshot, table.obj)
            .map_err(|error| error.to_string())?
        {
            if index.status != 1
                || index.expr_src.is_some()
                || index
                    .cols
                    .iter()
                    .any(|column| column.col == 0 || column.is_desc)
            {
                return Err("初始化模板暂不支持失效、表达式或降序索引".into());
            }
            let index_name = instance
                .catalog
                .resolve_by_obj(snapshot, index.obj)
                .map_err(|error| error.to_string())?
                .name;
            let keys = index
                .cols
                .iter()
                .map(|key| {
                    columns
                        .iter()
                        .find(|column| column.col == key.col)
                        .map(|column| quote(&column.name))
                        .ok_or("索引引用不存在的列")
                })
                .collect::<Result<Vec<_>, _>>()?;
            script.push_str(&format!(
                "CREATE {}INDEX {} ON {} ({});\n",
                if index.is_unique { "UNIQUE " } else { "" },
                quote(&index_name),
                quote(&table.name),
                keys.join(", ")
            ));
        }
    }
    let graphs = {
        let seq = instance.seq();
        let mut session = Session::new(instance.pool, instance.engine, &mut instance.catalog, seq);
        let (graph_sql, graphs) = session
            .graph_initialization_snapshot(graph_data)
            .map_err(|e| e.to_string())?;
        script.push_str(&graph_sql);
        graphs
    };
    if script.is_empty() {
        return Err("源工作区没有业务结构，不能创建空模板".into());
    }
    validate_schema(&script)?;
    if graph_data {
        validate_snapshots(&script, &graphs)?;
    }
    Ok((script, graphs))
}

/// Remove an immutable template, without touching any cloned workspace.
pub fn remove(home: &Path, name: &str) -> Result<(), String> {
    load(home, name)?;
    fs::remove_dir_all(directory(home, name)?).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::validate_schema;
    #[test]
    fn graph_template_whitelist_rejects_writes_external_targets_and_name_collisions() {
        assert!(validate_schema("CREATE GRAPH kg; CREATE FULLTEXT GRAPH INDEX words ON kg NODES (name) OPTIONS '{\"update\":\"manual\"}'; ALTER FULLTEXT GRAPH INDEX words ON kg PAUSE;").is_ok());
        for sql in [
            "CREATE GRAPH kg; CYPHER kg 'CREATE (n)';",
            "CREATE GRAPH kg; DROP GRAPH kg;",
            "CREATE GRAPH kg; ALTER GRAPH kg REBUILD ACCESS;",
            "CREATE GRAPH kg; CREATE GRAPH INDEX ix ON other NODES (name);",
            "CREATE GRAPH kg; ALTER FULLTEXT GRAPH INDEX words ON kg PAUSE;",
            "CREATE GRAPH kg; CREATE FULLTEXT GRAPH INDEX words ON kg NODES (name) OPTIONS '{\"update\":\"manual\"}'; ALTER FULLTEXT GRAPH INDEX words ON other PAUSE;",
            "CREATE GRAPH kg; CREATE GRAPH INDEX words ON kg NODES (name); ALTER FULLTEXT GRAPH INDEX words ON kg PAUSE;",
            "CREATE GRAPH kg; CREATE FULLTEXT GRAPH INDEX words ON kg NODES (name) OPTIONS '{\"update\":\"manual\"}'; ALTER FULLTEXT GRAPH INDEX words ON kg SYNC;",
            "CREATE GRAPH kg; CREATE FULLTEXT GRAPH INDEX words ON kg NODES (name) OPTIONS '{\"update\":\"manual\"}'; ALTER FULLTEXT GRAPH INDEX words ON kg OPTIONS '{\"update\":\"batch\"}';",
            "CREATE TABLE t(id NUMBER); CREATE GRAPH t;",
            "CREATE GRAPH kg; CREATE TABLE t(id NUMBER); CREATE INDEX words ON t(id); CREATE GRAPH INDEX words ON kg NODES (name);",
            "CREATE GRAPH kg; CREATE INDEX bad ON kg(ordinal);",
            "CREATE GRAPH kg; INSERT INTO kg VALUES (1,'data');",
            "CREATE USER attacker IDENTIFIED BY 'password' USING WORKSPACE kg;",
        ] {assert!(validate_schema(sql).is_err(),"{sql}");}
        let over_limit = (0..513)
            .map(|i| format!("CREATE GRAPH g{i};"))
            .collect::<String>();
        assert!(validate_schema(&over_limit).is_err());
    }
}
