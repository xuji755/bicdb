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
            "  脚本与输出    START <文件> / @<文件> / @@<文件> / SPOOL <文件|OFF|OUT> / PROMPT / REM".to_owned(),
            "  对象与诊断    DESCRIBE(DESC) <对象> / HOST !<命令> / TIMING [ON|OFF]".to_owned(),
            "  变量          DEFINE <名>=<值> / UNDEFINE <名> / &名（&&名 记住）".to_owned(),
            "  流程控制      WHENEVER SQLERROR EXIT|CONTINUE / EXIT(QUIT) [码]".to_owned(),
            "  帮助          HELP [主题]".to_owned(),
        ];
    }
    match w.as_str() {
        "set" | "show" => vec![
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
            "  只支持**直连**（列定义不外出）；服务在跑时先 `bicdb stop`".to_owned(),
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
