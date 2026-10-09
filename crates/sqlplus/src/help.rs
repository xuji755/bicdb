//! **`HELP` 文本**（SQL*Plus 的 `HELP [主题]` 形态；只列**已实现**的命令）。

/// 主题清单（`HELP`）。
#[must_use]
pub fn topics(which: &str) -> Vec<String> {
    let w = which.trim().to_ascii_lowercase();
    if w.is_empty() {
        return vec![
            "bicdbcli —— SQL*Plus 形态的客户端（`HELP <主题>` 看细节）".to_owned(),
            String::new(),
            "  SQL 输入      一行或多行，`;` 结尾执行；`/` 重跑当前缓冲；空行结束输入不执行".to_owned(),
            "  缓冲区        LIST(L) / RUN(R) / DEL / APPEND(A) / INPUT(I) / CHANGE(C) / CLEAR BUFFER".to_owned(),
            "  会话参数      SET <参数> <值> / SHOW [参数]".to_owned(),
            "  表目录        SHOW TABLES（当前工作区，包含字典表、固定表及附件虚拟表）".to_owned(),
            "  会话附件      HELP ATTACHMENTS（attachment$ 只读目录 / attachment_grep 文本检索）".to_owned(),
            "  属性图        HELP GRAPH（CYPHER / 图属性索引 / PROFILE）".to_owned(),
            "  脚本与输出    START <文件> / @<文件> / @@<文件> / SPOOL <文件|OFF|OUT> / PROMPT / REM".to_owned(),
            "  对象与诊断    DESCRIBE(DESC) <对象> / HOST !<命令> / TIMING [ON|OFF]".to_owned(),
            "  变量          DEFINE <名>=<值> / UNDEFINE <名> / &名（&&名 记住）".to_owned(),
            "  流程控制      WHENEVER SQLERROR EXIT|CONTINUE / EXIT(QUIT) [码]".to_owned(),
            "  帮助          HELP [主题]".to_owned(),
        ];
    }
    match w.as_str() {
        "graph" | "graphs" | "cypher" => vec![
            "属性图 —— 当前工作区内解析；命名用户在 PUBLIC 只读".into(),
            "  CREATE GRAPH kg;  SHOW GRAPHS;  DROP GRAPH kg;".into(),
            "  CYPHER kg 'MATCH (n:Entity) RETURN n.name AS name';".into(),
            "  CREATE GRAPH INDEX ix ON kg NODES LABEL \"Entity\" (db_type,config.port);".into(),
            "  CREATE UNIQUE GRAPH INDEX names ON kg NODES LABEL \"Entity\" (name);".into(),
            "  CREATE GRAPH INDEX links ON kg RELATIONSHIPS TYPE \"LINK\" (weight);".into(),
            "  ALTER GRAPH kg REBUILD ACCESS：原子重建内部节点目录与正反邻接树".into(),
            "  ALTER GRAPH kg REBUILD STORAGE：原子重建 v3 物理邻接路由，保留旧快照".into(),
            "  ALTER GRAPH kg UPGRADE STORAGE：原子迁移 heap 边到 v3 邻接段；只读查询不迁移".into(),
            "  SHOW GRAPH INDEXES ON kg;  ALTER GRAPH INDEX ix ON kg REBUILD;".into(),
            "  CREATE FULLTEXT GRAPH INDEX ft ON kg NODES LABEL \"Entity\" (name,config.answers[0]) OPTIONS '{\"update\":\"manual\"}';".into(),
            "  SHOW FULLTEXT GRAPH INDEXES ON kg;  ALTER FULLTEXT GRAPH INDEX ft ON kg SYNC;".into(),
            "  SEARCH FULLTEXT GRAPH INDEX ft ON kg FOR 'buffer pool' OPTIONS '{\"db_type\":\"mariadb\",\"consistency\":\"eventual\"}';".into(),
            "  ALTER FULLTEXT GRAPH INDEX ft ON kg PAUSE;  ALTER FULLTEXT GRAPH INDEX ft ON kg RESUME;".into(),
            "  ALTER FULLTEXT GRAPH INDEX ft ON kg WAIT OPTIONS '{\"timeout_ms\":30000}';：固定目标，返回 READY/TIMEOUT/PAUSED 与 target_reached；不是等候未来写入".into(),
            "  ALTER FULLTEXT GRAPH INDEX ft ON kg OPTIONS '{\"update\":\"batch\",\"interval_ms\":5000,\"batch_rows\":256}';".into(),
            "  CYPHER kg 'CALL db.index.fulltext.queryNodes(\"ft\",\"buffer\",{db_type:\"mariadb\",consistency:\"eventual\"}) YIELD node,score WHERE score>0 RETURN node.name,score';".into(),
            "  关系过程为 db.index.fulltext.queryRelationships；YIELD relationship/score/片段/水位，后续 MATCH 可继续遍历".into(),
            "  bicdb.searchNodes(查询,{db_type:领域,indexes:[索引名…],labels:[标签…],consistency:\"eventual\"})：按索引排名轮转去重；未指定 indexes 时仍扫描".into(),
            "  indexes:\"auto\",fields:[字段名…]：按字段顺序自动选择覆盖全部请求标签的节点全文索引；缺少覆盖报错".into(),
            "  DROP FULLTEXT GRAPH INDEX ft ON kg；节点/关系全文，固定 JSON 路径，按域复核源修订".into(),
            "  SELECT g.name FROM GRAPH_TABLE(kg, 'MATCH (n) RETURN n.name' COLUMNS (name VARCHAR2(128))) g;".into(),
            "  GRAPH_TABLE 只读；COLUMNS 按 RETURN 位置声明标量类型；PARAMETERS 可用命名参数。".into(),
            "  DROP GRAPH INDEX ix ON kg;  PROFILE CYPHER kg 'MATCH (n:Entity {name:$name}) RETURN n' PARAMETERS '{\"name\":\"sample\"}';".into(),
            "  SQL 未引号名称折叠小写；Cypher 标签/属性区分大小写，Entity 等名称需双引号".into(),
            "  索引 DDL 不能在活动事务中执行；PROFILE 只执行只读查询，并显示实际候选读取".into(),
            "  属性 B-tree 支持等值前缀及范围、JSON 对象路径；全文显式 manual/batch；batch 服务定时维护，OPTIONS 配置 interval_ms/batch_rows；SYNC 消费队列，REBUILD 全量换段".into(),
            "  全文默认 strict_scan：按语句可见源重建完整结果；eventual 使用倒排 B-tree，遗漏未同步变化但不返回陈旧源".into(),
        ],
        "attachments" | "attachment" => vec![
            "会话附件 —— 必须 -U 用户认证并连接本人的私有工作区".into(),
            "  SELECT filename, session_id, origin, searchable FROM attachment$;".into(),
            "  SELECT filename, line_number, byte_offset, snippet FROM attachment_grep('IO');".into(),
            "  attachment_grep(pattern [, session_id [, attachment_id]])：可用 :命名参数；空 ID 表示不限制".into(),
            "  精确、区分大小写的 UTF-8 字面子串扫描；不是正则或全文索引；searchable 表示支持的文本格式候选".into(),
            "  HTML/MD/TXT/LOG/SQL/CSV/JSON/XML/SVG：搜索原文；PDF、图片、压缩包尚不支持解析".into(),
            "  SHA-256 校验后才返回结果；超 256 文件 / 256 MiB / 1000 命中 / 扫描 10 秒时报错，不静默截断".into(),
            "  当前用户当前工作区内全部会话；跨会话仅此显式 SQL 查询，不自动注入智能体上下文".into(),
        ],
        "set" | "show" => vec![
            "  SHOW TABLES              列出当前工作区表名、类型、对象号；不授予字典正文访问"
                .to_owned(),
            "SET / SHOW —— 会话参数（只列有落点的）".to_owned(),
            "  SET ECHO ON|OFF          执行前回显缓冲".to_owned(),
            "  SET FEEDBACK ON|OFF      结果集后的 `N rows selected.`".to_owned(),
            "  SET TIMING ON|OFF        每条语句后的 `Elapsed: …`".to_owned(),
            "  SET PAGESIZE n           每 n 行翻页并重打表头（0 = 不分页）".to_owned(),
            "  SET LINESIZE n           结果集总宽上限".to_owned(),
            "  SET NULL <文本>          NULL 的显示形态（默认空）".to_owned(),
            "  SET TERMOUT ON|OFF       脚本执行时是否上屏幕（SPOOL 照收）".to_owned(),
            "  SET SQLPROMPT <文本>     主提示符（默认 `SQL> `）".to_owned(),
            "  SET VERIFY ON|OFF        替换变量替换后回显".to_owned(),
            "  SET CONCAT <字符>        替换变量前缀（默认 `&`）".to_owned(),
            "  SHOW [参数]              看当前取值（`SHOW SPOOL` 看假脱机状态）".to_owned(),
        ],
        "spool" => vec![
            "SPOOL —— 把之后的输出同时写进文件".to_owned(),
            "  SPOOL <文件>   开（会截断同名文件）".to_owned(),
            "  SPOOL OFF      关".to_owned(),
            "  SPOOL OUT      关并报告文件名".to_owned(),
            "  `SET TERMOUT OFF` 时屏幕安静、**文件照收**（SQL*Plus 同款分工）".to_owned(),
        ],
        "start" | "@" => vec![
            "START / @ / @@ —— 跑脚本".to_owned(),
            "  START <文件>   逐行按交互同一套规则处理（命令 + SQL 都能写）".to_owned(),
            "  @<文件>        同上".to_owned(),
            "  @@<文件>       相对**当前脚本所在目录**解析".to_owned(),
            "  脚本里 SQL 失败默认继续；`WHENEVER SQLERROR EXIT` 改为中断并传退出码".to_owned(),
        ],
        "describe" | "desc" => vec![
            "DESCRIBE(DESC) <对象> —— 列名/可空/类型（SQL*Plus 版面）".to_owned(),
            "  服务和直连均支持；列可见性遵循当前连接身份（包含 attachment$）".to_owned(),
        ],
        "connect" | "login" => vec![
            "连接 —— 由启动参数决定，不做 `CONNECT`".to_owned(),
            "  bicdbcli <实例目录>            服务在跑 ⇒ 经套接字；否则直连".to_owned(),
            "  bicdbcli --direct <实例目录>   强制直连（服务在跑会被锁挡住）".to_owned(),
            "  单写者纪律：同一实例同一时刻只允许一个写者（直连或服务）".to_owned(),
        ],
        other => vec![format!("!  没有主题 `{other}`（试 `HELP`）")],
    }
}
