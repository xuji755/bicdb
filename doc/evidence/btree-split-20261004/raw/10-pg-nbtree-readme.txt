候选覆盖有界：当前结果按召回或显示预算选取，未宣称全库穷尽。

============================================================
  [问题诊断] nbtree README 页 结构 高键 分裂 两阶段
============================================================
  三路召回 → name/summary 重排 → 正文按需读取（绝对相关性过滤，无相对分差淘汰）
  FaultModel：已取 8，摘要筛选通过 2，批次 1；本路已取尽
  KnowledgeEntry：已取 64，摘要筛选通过 62，批次 1；相关证据已满足，仍有未取候选
  CaseStudy：已取 0，摘要筛选通过 0，批次 1；本路已取尽
  正文核实 0 条；最终正文补读 6 条

  ✅ 答案要点
     nbtree README指出删除叶页分两阶段：第一阶段从父节点unlink并标记为half-dead；第二阶段依次锁定左兄弟、目标页、右兄弟，更新兄弟side-link并将目标页标记为deleted。只有当索引页完全为空时才考虑删除整页。

  📍 诊断结果 (6条, 已过滤低相关<5分)

     [KnowledgeEntry] nbtree索引页删除的两阶段过程  (排序分: 10.0)
     └ 摘要：nbtree README指出删除叶页分两阶段：第一阶段从父节点unlink并标记为half-dead；第二阶段依次锁定左兄弟、目标页、右兄弟，更新兄弟side-link并将目标页标记为deleted。只有当索引页完全为空时才考虑删除整页。
     详情请见：python3 kb_graph.py postgresql detail 4:8ed6c541-fe08-4ade-ae68-a79bf45316f4:2298016

     [KnowledgeEntry] PostgreSQL README hash index  (排序分: 10.0)
     └ 详情：PostgreSQL README hash index：该README文件描述了Hash索引的增量扩展、页面类型、分裂机制等实现细节。
分类：工具
     详情请见：python3 kb_graph.py postgresql detail 4:8ed6c541-fe08-4ade-ae68-a79bf45316f4:558277

     [KnowledgeEntry] btree索引结构  (排序分: 10.0)
     └ 详情：btree索引结构：PostgreSQL的B-tree索引是一种平衡树结构，用于支持等值和范围查询，参考src/backend/access/nbtree/README。
分类：原理
     详情请见：python3 kb_graph.py postgresql detail 4:8ed6c541-fe08-4ade-ae68-a79bf45316f4:892895

     [KnowledgeEntry] BRIN扩展阅读：源码README  (排序分: 10.0)
     └ 详情：BRIN扩展阅读：源码README：该条目提供了BRIN源码README路径：postgres/src/backend/access/brin/README。诊断时，该README文件提供了BRIN模块的设计概述和架构说明，适合开发人员快速上手。
分类：工具
     详情请见：python3 kb_graph.py postgresql detail 4:8ed6c541-fe08-4ade-ae68-a79bf45316f4:479707

     [KnowledgeEntry] GiST README文档  (排序分: 10.0)
     └ 详情：GiST README文档：GiST README文档是PostgreSQL源码中关于GiST的内部文档，学习GiST实现的重要参考。
分类：工具
     详情请见：python3 kb_graph.py postgresql detail 4:8ed6c541-fe08-4ade-ae68-a79bf45316f4:523317

     [FaultModel] 拿README性能数字做容量规划  (排序分: 9.5)
     └ 摘要：拿 README 性能数字做容量规划，直接引用项目文档中的数字，未考虑自身数据、查询、硬件差异。实际性能与预期偏差大。应基于实际环境进行基准测试。
     详情请见：python3 kb_graph.py postgresql detail 4:8ed6c541-fe08-4ade-ae68-a79bf45316f4:1390528

