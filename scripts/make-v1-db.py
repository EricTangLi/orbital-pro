"""造一个 v1 形态的旧库（带数据），用于 T-M6-03 的"安装版读旧库"验证。

SCHEMA_V1 直接从 crates/orbital-core/src/schema.rs 里抽（不手抄，避免漂移）。
"""
import re
import sqlite3
import sys
from pathlib import Path

schema_rs = Path(sys.argv[1]).read_text(encoding="utf-8")
m = re.search(r'pub const SCHEMA_V1: &str = r#"(.*?)"#;', schema_rs, re.S)
if not m:
    raise SystemExit("找不到 SCHEMA_V1")
v1_ddl = m.group(1)

db_path = Path(sys.argv[2])
db_path.unlink(missing_ok=True)
c = sqlite3.connect(db_path)
c.executescript(v1_ddl)
c.execute(
    "INSERT INTO memories (id, content, agent_id, created_at, updated_at, valid_from,"
    " recorded_at, content_hash) VALUES ('m1', 'v1 时代的旧数据', 'legacy', 1000, 1000, 1000, 1000, 'h1')"
)
c.execute(
    "INSERT INTO memory_events (operation, memory_id, actor, detail, created_at)"
    " VALUES ('write', 'm1', 'legacy', '{}', 1000)"
)
c.execute("PRAGMA user_version = 1")
c.commit()
print("made v1 db:", db_path, "user_version=1, memories=1")
c.close()
