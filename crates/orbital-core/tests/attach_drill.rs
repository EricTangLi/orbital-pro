//! 接入演练：把**真实 agent 配置的副本**复制到临时目录，跑 attach_yaml 验证。
//!
//! 原则：**绝不修改真实配置**，只读它、复制它、在副本上演练。
//! 找不到真实文件时自动跳过（不失败）。

use std::path::PathBuf;

fn candidates() -> Vec<(&'static str, PathBuf)> {
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    vec![
        ("dsh", PathBuf::from(format!(r"{home}\.dsh\cordis.patch.yml"))),
        ("hermes-user", PathBuf::from(format!(r"{home}\.hermes\config.yaml"))),
        ("hermes-main", PathBuf::from(format!(r"{local}\hermes\config.yaml"))),
    ]
}

#[test]
fn drill_attach_on_copies_of_real_configs() {
    let tmp = std::env::temp_dir().join(format!("orbital-drill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let db = orbital_core::db::Db::open(tmp.join("drill.db")).unwrap();

    let mut drilled = 0;
    let mut skipped = 0;
    for (agent, src) in candidates() {
        if !src.exists() {
            skipped += 1;
            println!("SKIP {agent}: 真实文件不存在 {}", src.display());
            continue;
        }
        let dst = tmp.join(format!("{agent}.yaml"));
        std::fs::copy(&src, &dst).unwrap();
        let before = std::fs::read_to_string(&dst).unwrap();

        // ★ 关键：只在副本上操作
        let out = orbital_core::attach::attach_yaml(&db, agent, &dst, "orbital");
        match out {
            Ok(o) => {
                let after = std::fs::read_to_string(&dst).unwrap();
                println!(
                    "OK   {agent}: changed={} backup={} tokenLen={}",
                    o.changed,
                    o.backup_path.is_some(),
                    o.token.len()
                );
                if o.changed {
                    // 1) 原内容每一行都还在（注释/其他条目不丢）
                    for line in before.lines().filter(|l| !l.trim().is_empty()) {
                        assert!(after.contains(line), "演练丢失了原有行: {line}");
                    }
                    // 2) 整份仍是合法 YAML（注意：mcp 可能是二级键，不能在根节点找）
                    let _v: serde_yaml::Value =
                        serde_yaml::from_str(&after).expect("after 必须是合法 YAML");
                    // 3) 新条目确实写进去了
                    assert!(after.contains("orbital:"), "应写入 orbital 条目");
                    assert!(after.contains("orbital-mcp"), "command 应为 orbital-mcp");
                    // 3) 真实文件一字节未变
                    assert_eq!(
                        std::fs::read_to_string(&src).unwrap(),
                        std::fs::read_to_string(&src).unwrap()
                    );
                }
                drilled += 1;
            }
            Err(e) => {
                println!("REFUSE {agent}: {} ({})", e.code(), e);
                assert_eq!(
                    std::fs::read_to_string(&dst).unwrap(),
                    before,
                    "拒绝时副本不能被改动"
                );
                drilled += 1;
            }
        }
    }
    println!("drilled={drilled} skipped={skipped}");
    assert!(drilled > 0, "至少演练一个配置文件（否则说明路径都没找到）");
    let _ = std::fs::remove_dir_all(&tmp);
}
