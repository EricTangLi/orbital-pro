# -*- coding: utf-8 -*-
"""把记忆的创建/访问时间改成不同老化程度，用于退役候选对照（T-M9-01）。

用法：python age-fixture.py <db路径>
"""
import sqlite3
import sys
import time

db = sys.argv[1]
now = int(time.time())
c = sqlite3.connect(db)
rows = c.execute("SELECT id FROM memories ORDER BY created_at LIMIT 50").fetchall()
# ⚠️ 上一版把 days 与 access 写成同余（[60,180,365][i%3] 配 [0,1,3][i%3]）：
#   "60 天档"必然 access=0、"365 天档"必然 access=3（后者全被免疫规则筛掉）
#   -> 两档半衰期结果完全相同（34 vs 34），等于自己把差异抹平。
# 现在：**天数 4 档 × 访问 3 档交叉**，让 ei 落在阈值（0.35）附近，才能看出参数影响。
DAYS = [45, 70, 110, 200]
ACCESS = [0, 1, 2]  # 3 会被免疫规则直接排除，这里不用
for i, (mid,) in enumerate(rows):
    days = DAYS[(i // 3) % 4]
    access = ACCESS[i % 3]
    created = now - days * 86400
    last = now - max(days - 5, 1) * 86400
    c.execute(
        "UPDATE memories SET created_at=?, valid_from=?, recorded_at=?, last_accessed_at=?, access_count=? WHERE id=?",
        (created, created, created, last, access, mid),
    )
c.commit()
print("aged", len(rows), "rows: days 45/70/110/200 x access 0/1/2 (crossed, not congruent)")
c.close()
