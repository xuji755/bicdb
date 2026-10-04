# 外部检索摘要：PG 取消协议与 cancel/terminate（2026-10-04，1 轮 WebSearch）

## BackendKeyData 与 CancelRequest（协议）

- **BackendKeyData（'K'）**：连接建立（认证通过后）由服务端下发——消息含
  **PID + 秘密键**（协议 3.2 前秘密键固定 4 字节，上限 256 字节）。前端**必须保存**以备取消。
- **CancelRequest**：取消**另开一条新连接**发送——长度 16、**取消码 80877102**、目标 PID、秘密键；
  **键数据不符即忽略**（防未授权第三方取消）；
- **语义限制**：取消信号**可能完全无效**（如查询已结束）；有效时当前命令以错误结束；
  **前端不能直接知道取消是否成功**——要靠主连接上的响应来判定。

## pg_cancel_backend vs pg_terminate_backend

- **协作式**：两者都依赖后端到达 `CHECK_FOR_INTERRUPTS()` 安全点（取元组之间、表达式求值中、访问方法边界……）。
- **`pg_cancel_backend(pid)`**：发 **SIGINT**，标记 pending 取消——不会立即停止；复杂节点（大哈希、排序、物化写临时文件）
  可能很久到不了检查点——"简单查询秒退、分析型查询像免疫"。
- **`pg_terminate_backend(pid)`**：发 **SIGTERM**（`ProcDiePending`）——**优先级高于取消**；
  到达检查点后放弃正常执行、紧急收尾：**事务中止、锁释放、会话终止**；清理**异步**完成；对连接池有破坏性，仅显式请求时使用。
- **权限**：superuser / `pg_signal_backend` 成员 / 与被断开者同角色的用户（`kill -SIGTERM` 等效，但不推荐；**绝不用 `kill -9`**）。

链接：
- https://git.postgresql.org/cgit/pgweb-static.git/plain/documentation/pdf/15/postgresql-15-A4.pdf （协议：CancelRequest / BackendKeyData）
- https://www.cybrosys.com/research-and-development/postgres/how-pgcancelbackend-and-pgterminatebackend-work
- https://public.dalibo.com/exports/formation/manuels/modules/h2/h2.handout.pdf （管理函数清单）
