//! Evidence → Memory 合并/提升引擎 (Phase 2 Consolidation)。
//!
//! 流程:
//! 1. 扫描所有 active evidence,按 content_hash 前缀分组 (相似内容归桶)
//! 2. ≥ MIN_OCCURRENCES 相同或近似结论 → 形成稳定 Memory
//! 3. 特殊模式快速提升: 同一用户问题重复 ≥2 次 = Preference
//! 4. 通过 ConsolidationTx 状态机保障可恢复
//! 5. 提升后: 为 source evidence 标记 superseded, 创建 Supports/DerivedFrom edges
//!
//! 设计原则: 保守提升,不做模糊聚类 (NLP 调用另走 P1)。当前版本仅基于
//! 内容哈希精确分桶 + 模式匹配, 保证每一条 Promotion 都可审计、可重现。

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::database::{DbError, MemoryDatabase};
use crate::indexer::{ConsolidationState, ConsolidationTx};
use crate::types::*;

/// 同组内 evidence 数达到此阈值才创建 Memory。
/// P1 修复(W3 消化阀门): 3 → 2，与真实生产数据分布对齐（历史
/// 809 组里只有 1 组达到 3 条，绝大多数最大 2 条）。
const MIN_OCCURRENCES: usize = 2;
/// 单次 consolidate 最多提升多少组,避免冷启动爆冲。
const MAX_PROMOTIONS_PER_RUN: usize = 20;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ConsolidationReport {
    pub groups_evaluated: usize,
    pub groups_promoted: usize,
    pub groups_insufficient: usize,
    pub memories_created: usize,
    pub evidence_superseded: usize,
    pub edges_created: usize,
    pub errors: usize,
}

impl MemoryDatabase {
    /// 运行一次 consolidation 流程: 从 Evidence 提升稳定 Memory。
    ///
    /// 幂等: 已被 superseded 的 evidence 跳过。可在启动后台或周期性
    /// 任务中多次调用。返回统计结果供诊断日志。
    pub fn run_consolidation_pass(&self) -> Result<ConsolidationReport, DbError> {
        let mut report = ConsolidationReport::default();

        // 先崩溃恢复: PREPARED/DB_APPLIED → FAILED (不删 unit)
        let _ = self.recover_nonterminal_txs()?;

        // W3 修复（content 归一化分桶）：旧版按 evidence_units.content_hash
        // 精确前缀分桶，同义/近义（大小写、空格、标点差异）永远独立桶，
        // 使得 MIN_OCCURRENCES=2 仍难以触发。现在先拉出全部 active
        // evidence，对 content 做归一化（trim + lower + 去标点 + 空白压缩）
        // 再 SHA256 前缀作为桶 key，另外允许 remember/call me/我是 模式
        // 的 UserQuestion 组在 ≥1 条时直接升为 Preference 兜底。
        let all = self.list_active_evidence_flat()?;
        let groups = group_evidence_by_normalized_content(&all);
        report.groups_evaluated = groups.len();

        let mut promoted = 0usize;
        for (_hash_prefix, evidences) in groups {
            if promoted >= MAX_PROMOTIONS_PER_RUN {
                break;
            }

            // W3: Preference 兜底提升 — UserQuestion 组只要命中
            // 「叫我/记住我/call me/remember/我是/我叫」且 ≥1 条时
            // 直接升 Memory kind=Preference scope=Global，绕过
            // MIN_OCCURRENCES 门槛。
            let pref_fallback = is_preference_pattern(&evidences);
            if evidences.len() < MIN_OCCURRENCES && !pref_fallback {
                report.groups_insufficient += 1;
                continue;
            }

            // 跳过已有 superceding memory 的组。
            let superseded_by_any = evidences
                .iter()
                .filter_map(|e| {
                    let supers = self.superseding_memories(&e.id).ok()?;
                    if supers.is_empty() { None } else { Some(()) }
                })
                .next()
                .is_some();
            if superseded_by_any {
                continue;
            }

            // 决定 Memory kind: 看证据 section 分布。
            // Preference 兜底时强制覆盖为 Preference（后续内容合成走 Preference 标签）。
            let kind = if pref_fallback {
                MemoryKind::Preference
            } else {
                decide_memory_kind(&evidences)
            };

            // 1) PREPARED tx
            let evidence_ids: Vec<String> = evidences.iter().map(|e| e.id.clone()).collect();
            let manifest = serde_json::json!({
                "evidence_ids": evidence_ids,
                "kind": kind.as_str(),
                "occurrence_count": evidences.len(),
                "rollout_ids": evidences.iter().map(|e| e.rollout_id.clone()).collect::<Vec<_>>(),
            });
            let input_hash = {
                let mut hasher = Sha256::new();
                hasher.update(manifest.to_string().as_bytes());
                format!("{:x}", hasher.finalize())
            };
            let tx_id = format!("consol_{}", Uuid::new_v4());
            let tx = ConsolidationTx::new_prepared(
                tx_id.clone(),
                None,
                None,
                input_hash.clone(),
                manifest.to_string(),
            );
            if let Err(e) = self.create_consolidation_tx(&tx) {
                eprintln!("[warn] consolidation create_tx failed: {e}");
                report.errors += 1;
                continue;
            }

            // 2) 合成 MemoryUnit: 用第一条 evidence 的内容,加上来源汇总。
            let mem_content = compose_memory_content(&evidences, kind);
            // W3 Preference 兜底时强制 scope=Global（跨工作区用户偏好），
            // 否则沿用默认的 Global→Workspace 回退。
            let scope = if pref_fallback {
                MemoryScope::Global
            } else {
                evidences
                    .iter()
                    .find(|e| matches!(e.scope, MemoryScope::Global))
                    .map(|e| e.scope)
                    .unwrap_or(MemoryScope::Workspace)
            };
            let mem_id = make_consolidation_mem_id(&input_hash);
            let mem_content_hash = {
                let mut h = Sha256::new();
                h.update(mem_content.as_bytes());
                format!("{:x}", h.finalize())
            };
            let now = Utc::now();
            let mu = MemoryUnit {
                id: mem_id.clone(),
                path: String::from("__consolidated__"),
                section: compose_memory_section(&evidences),
                kind,
                scope,
                status: UnitStatus::Active,
                content: mem_content,
                content_hash: mem_content_hash.clone(),
                updated_at: now,
                created_at: now,
            };
            if let Err(e) = self.upsert_memory_unit(&mu) {
                eprintln!("[warn] consolidation upsert_memory failed: {e}");
                let _ = self.transition_consolidation_tx(&tx_id, ConsolidationState::Failed, None);
                report.errors += 1;
                continue;
            }

            // 3) DB_APPLIED transition
            if let Err(e) = self.transition_consolidation_tx(
                &tx_id, ConsolidationState::DbApplied, Some(&mem_content_hash)
            ) {
                eprintln!("[warn] consolidation DB_APPLIED failed: {e}");
                report.errors += 1;
                continue;
            }

            // 4) Provenance edges: DerivedFrom + Supports for every evidence
            for ev in &evidences {
                let _ = self.add_provenance_edge(&ProvenanceEdge {
                    memory_id: mem_id.clone(),
                    evidence_id: ev.id.clone(),
                    relation: EdgeRelation::DerivedFrom,
                    created_at: Utc::now(),
                });
                let _ = self.add_provenance_edge(&ProvenanceEdge {
                    memory_id: mem_id.clone(),
                    evidence_id: ev.id.clone(),
                    relation: EdgeRelation::Supports,
                    created_at: Utc::now(),
                });
                report.edges_created += 2;
                // 5) 标记 Supersedes edge + supersede evidence
                let _ = self.add_provenance_edge(&ProvenanceEdge {
                    memory_id: mem_id.clone(),
                    evidence_id: ev.id.clone(),
                    relation: EdgeRelation::Supersedes,
                    created_at: Utc::now(),
                });
                report.edges_created += 1;
                if self.supersede_evidence(&ev.id, &mem_id).is_ok() {
                    report.evidence_superseded += 1;
                }
            }

            // 6) COMPLETED
            let _ = self.transition_consolidation_tx(&tx_id, ConsolidationState::Completed, None);
            promoted += 1;
            report.groups_promoted += 1;
            report.memories_created += 1;
        }

        Ok(report)
    }
}

fn compose_memory_content(evs: &[EvidenceUnit], kind: MemoryKind) -> String {
    if evs.is_empty() { return String::new(); }
    let main = &evs[0].content;
    let mut out = String::new();
    let kind_label = match kind {
        MemoryKind::Preference => "[Stable Preference]",
        MemoryKind::Decision => "[Stable Decision]",
        MemoryKind::Constraint => "[Stable Constraint]",
        MemoryKind::Solution => "[Stable Solution]",
        MemoryKind::Fact => "[Stable Fact]",
    };
    out.push_str(kind_label);
    out.push('\n');
    out.push_str(main);
    out.push_str("\n\n");
    out.push_str(&format!(
        "Confirmed across {} historical sessions ({} distinct). Sources:",
        evs.len(),
        evs.iter()
            .map(|e| e.rollout_id.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len()
    ));
    out.push('\n');
    for (i, e) in evs.iter().enumerate().take(8) {
        out.push_str(&format!(
            "  [{}] session {} ({})",
            i,
            &e.rollout_id[..std::cmp::min(8, e.rollout_id.len())],
            e.occurred_at.format("%Y-%m-%d"),
        ));
        out.push('\n');
    }
    out
}

fn compose_memory_section(evs: &[EvidenceUnit]) -> String {
    if evs.is_empty() { return String::new(); }
    let sections: std::collections::HashSet<&str> = evs
        .iter()
        .map(|e| e.section.as_str())
        .filter(|s| !s.is_empty())
        .collect();
    if sections.is_empty() {
        return format!("consolidated ({} evidences)", evs.len());
    }
    format!(
        "consolidated from: {}",
        sections.into_iter().take(3).collect::<Vec<_>>().join(", ")
    )
}

fn decide_memory_kind(evs: &[EvidenceUnit]) -> MemoryKind {
    use std::collections::HashMap;
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for e in evs {
        let sec = e.section.as_str();
        let hint = if sec.contains("用户问题") || sec.contains("偏好") || sec.contains("preference") {
            "preference"
        } else if sec.contains("修复") || sec.contains("fix") || sec.contains("solution") {
            "solution"
        } else if sec.contains("tool_result") {
            // tool 错误结论: 常被提升为解决方案/约束
            let c = &e.content;
            if c.contains("missing") || c.contains("not found") || c.contains("failed to link")
                || c.contains("error") {
                "solution"
            } else if c.contains("must") || c.contains("必须") || c.contains("ensure") {
                "constraint"
            } else {
                "fact"
            }
        } else {
            "fact"
        };
        *counts.entry(hint).or_insert(0) += 1;
    }
    let best = counts.into_iter().max_by_key(|(_, n)| *n).map(|(k, _)| k);
    match best.unwrap_or("fact") {
        "preference" => MemoryKind::Preference,
        "solution" => MemoryKind::Solution,
        "constraint" => MemoryKind::Constraint,
        "decision" => MemoryKind::Decision,
        _ => MemoryKind::Fact,
    }
}

fn make_consolidation_mem_id(input_hash: &str) -> String {
    // mem_c_{input_hash[:10]}
    format!("mem_c_{}", &input_hash[..std::cmp::min(10, input_hash.len())])
}

// ── W3 helpers: normalized content bucketing + preference fallback ──────

/// 对 evidence content 做归一化：trim → 中文/英文 lower → 去标点/符号 → 压缩空白。
///
/// 不做同义词/语义合并（保守），但能把 "叫我 9527！"、"叫我 9527"、
/// "叫我9527" 合进同一桶，显著减少 MIN_OCCURRENCES 门控的误判。
fn normalize_content_for_bucket(s: &str) -> String {
    fn is_cjk_symbol_or_punctuation(c: char) -> bool {
        // 全角标点/符号：常见 CJK 符号和标点区 (U+3000..U+303F)、
        // 全角 ASCII (FF00..FFEF) 里的非字母数字、CJK 标点
        // (U+2000..U+206F 通用标点里的常见号 + 顿号 U+3001、句号 U+3002
        // 、问号 U+FF1F/、感叹 U+FF01、逗号 U+FF0C、冒号 U+FF1A
        // 、分号 U+FF1B、引号 U+201C..201D)。
        matches!(
            c,
            '\u{3000}'..='\u{303F}' // CJK 符号标点
            | '\u{FF00}'..='\u{FFEF}' // 全角 ASCII/半角片假名（整体丢弃由 is_alphanumeric 过滤字母数字，这里再把非字母数字的全角标点符号范围单独包含）
            | '\u{2000}'..='\u{206F}' // 通用标点
            | '…' | '—' | '–' | '·' | '・'
            | '「' | '」' | '『' | '』'
            | '《' | '》' | '〈' | '〉'
            | '、' | '。' | '，' | '！' | '？' | '：' | '；'
            | '“' | '”' | '‘' | '’'
        )
        // 注：FF00-FFEF 里的全角字母/数字其实是"字母数字"，但它们 Unicode
        // category 已经是 Letter/Number，会被我们正常保留；只有全角标点才属
        // 于其他类，会落在下面的丢弃路径里。所以这里不单独判断。
    }

    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for ch in s.trim().chars() {
        let lower = ch.to_lowercase();
        for c in lower {
            if c.is_whitespace() || is_cjk_symbol_or_punctuation(c) {
                if !prev_space {
                    out.push(' ');
                    prev_space = true;
                }
            } else if c.is_ascii_alphanumeric()
                // CJK 统一表意汉字 + 扩展 A/B（保守粗覆盖，保证中日韩常用字保留）
                || matches!(c as u32, 0x4E00..=0x2A6DF | 0x3400..=0x4DBF)
                // 假名（日文）/韩文（Hangul Syllables）
                || matches!(c as u32, 0x3040..=0x30FF | 0xAC00..=0xD7AF)
                // ASCII 字母数字已经被 is_ascii_alphanumeric 覆盖；
                // 额外保留 ASCII 下划线 / 连字符 / @ / + / 等常见 token 字符，
                // 以便邮箱/ID 分桶稳定（不把 foo_bar 变成 foobar）。
                || matches!(c, '_' | '-' | '@' | '+' | '#')
            {
                out.push(c);
                prev_space = false;
            }
            // ASCII 标点 / emoji / 其他控制字符 丢弃
        }
    }
    out.trim().to_string()
}

/// 对扁平 evidence 列表按「content 归一化 + SHA256 前缀 16 hex」分桶。
/// 返回的每一组内 evidences 保持发生时序（外部 DB order by occurred_at）。
fn group_evidence_by_normalized_content(
    evs: &[EvidenceUnit],
) -> Vec<(String, Vec<EvidenceUnit>)> {
    use std::collections::HashMap;
    let mut groups: HashMap<String, Vec<EvidenceUnit>> = HashMap::new();
    for ev in evs {
        let normalized = normalize_content_for_bucket(&ev.content);
        if normalized.is_empty() {
            // 空归一化用原始 content_hash 前缀兜底，避免全部合并成一个桶
            let key: String = ev.content_hash.chars().take(16).collect();
            groups.entry(key).or_default().push(ev.clone());
            continue;
        }
        let mut h = Sha256::new();
        h.update(normalized.as_bytes());
        let hash = format!("{:x}", h.finalize());
        let key: String = hash.chars().take(16).collect();
        groups.entry(key).or_default().push(ev.clone());
    }
    groups.into_iter().collect()
}

/// W3 Preference 兜底：当 group 内任一 UserQuestion evidence 的 content
/// 命中「叫我/记住我/我是/我叫/我喜欢/my name is / call me / remember (that)?」
/// 等模式，即可把门槛从 MIN_OCCURRENCES=2 降到 1，并强制 kind=Preference、
/// scope=Global。
fn is_preference_pattern(evs: &[EvidenceUnit]) -> bool {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        // 注意：中文字符匹配必须保留 default Unicode 模式，不能写 (?im-u)。
        // 这里显式用 (?im)，每行 case-insensitive + 多行锚点。
        let pats: &[&str] = &[
            // 中文称呼
            r"(?im)^\s*(以后|之后)?\s*请?(把我|叫我|喊我|称呼我)\s*(做|为)?\s*[:：]?\s*.+",
            r"(?im)^\s*(我叫|我的名字是|我是)\s*(做|为)?\s*[:：]?\s*\S+",
            // 中文记住 + 喜欢/不喜欢
            r"(?im)^\s*(请?|麻烦|劳烦)?\s*记住?我(喜欢|偏好|比较?喜欢|更爱|最爱|不喜欢|讨厌|反感)\s*[:：]?\s*.+",
            // 我的 XX 是 XX
            r"(?im)^\s*(我)?\s*的\s*(名字|姓名|昵称|称呼|邮箱|邮件|email|手机|电话|微信|wechat|qq|tg|telegram|discord|github|用户名|id|工号|部门|职位|公司)\s*(是|为|=)\s*[:：]?\s*.+",
            // 自由记住
            r"(?im)^\s*(请?|麻烦|劳烦)?\s*(记下来|记住|mark一下|mark 一下|存一下|记好)\s*[:：]?\s*\S+",
            // 英文
            r"(?i)^\s*remember\s+(?:that\s+)?\S+",
            r"(?i)^\s*call\s+me\s+\S+",
            r"(?i)^\s*my\s+name\s+is\s+\S+",
        ];
        match regex::Regex::new(&pats.join("|")) {
            Ok(r) => r,
            Err(e) => {
                // Fail-closed: regex 构造失败时退化为「永不匹配」的空正则，
                // 绝不让 runtime panic 把整个 agent 弄挂。
                eprintln!("[consolidator] preference regex build failed (fallback=never-match): {e}");
                regex::Regex::new("a^").expect("empty negative-lookahead regex")
            }
        }
    });
    evs.iter().any(|e| {
        // 限定 UserQuestion / preference section 或 content 命中即可
        (e.section.contains("用户问题")
            || e.section.contains("用户偏好")
            || e.section.contains("preference")
            || e.section.contains("question"))
            && re.is_match(&e.content)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::apply_schema;
    use rusqlite::Connection;

    /// 防止再次回归：确保 is_preference_pattern 内部的大正则（含中文）
    /// 可以 parse；之前写 (?im-u) 禁用 Unicode 导致 runtime panic。
    /// 这是编译期 + test-time 双保险：构造一旦失败，单元测试直接报错。
    #[test]
    fn preference_regex_parses_without_unicode_conflict() {
        // Dummy evidence slice — 即便空，调用 is_preference_pattern 会
        // 走 OnceLock 的 get_or_init，触发 regex 构造（如果失败会 panic）。
        let empty: &[EvidenceUnit] = &[];
        assert!(!is_preference_pattern(empty));

        // 命中几个典型输入：确保中文/英文各模式都正确编译并匹配。
        let samples = [
            ("以后叫我 9527", "用户问题 summary"),
            ("记住我喜欢 Rust", "用户偏好 preference"),
            ("我的邮箱是 a@b.com", "用户问题 question"),
            ("Call me Alice", "用户问题 summary"),
            ("remember that I prefer dark mode", "用户问题 summary"),
        ];
        for (text, sec) in samples {
            let eu = EvidenceUnit {
                id: format!("ev_{}", text.len()),
                rollout_id: "s1".into(),
                path: "/tmp/x".into(),
                section: sec.into(),
                scope: MemoryScope::Workspace,
                status: EvidenceStatus::Active,
                content: text.into(),
                content_hash: "x".into(),
                occurred_at: chrono::Utc::now(),
                created_at: chrono::Utc::now(),
                superseded_by: None,
                superseded_at: None,
                rollout_available: true,
                rollout_expired_at: None,
                subchunk_index: 0,
            };
            assert!(
                is_preference_pattern(&[eu]),
                "is_preference_pattern should match: {text}"
            );
        }
    }

    #[test]
    fn normalize_content_bucket_collapses_punctuation_and_case() {
        // 全角叹号/句号应被去掉/折叠成空格，与无标点等价（两侧都无空格）
        assert_eq!(
            normalize_content_for_bucket("叫我9527！"),
            normalize_content_for_bucket("叫我9527")
        );
        // 即使用户写了 ！前后都没有空格，也不应在末尾留空格
        assert_eq!(normalize_content_for_bucket("叫我9527！"), "叫我9527");
        // 两侧各有空格时，合并为一个 token
        assert_eq!(normalize_content_for_bucket("叫我 9527！"), "叫我 9527");
        // ASCII 大小写与句号
        assert_eq!(
            normalize_content_for_bucket("Remember that I like Rust."),
            normalize_content_for_bucket("remember that i like rust")
        );
        // 多空白压缩（空格/Tab/NL → 单空格）
        assert_eq!(
            normalize_content_for_bucket("a   b\t\nc"),
            normalize_content_for_bucket("a b c")
        );
        // 邮箱 token 的 @ / _ 保留，避免误合并成同一桶
        assert_eq!(
            normalize_content_for_bucket("我的邮箱是 dev_foo@example.com"),
            "我的邮箱是 dev_foo@examplecom" // ASCII . 被当标点丢弃
        );
    }

    fn make_db() -> MemoryDatabase {
        let conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).unwrap();
        MemoryDatabase::from_conn(conn)
    }

    fn insert_evidence(db: &MemoryDatabase, content: &str, rollout: &str, section: &str) -> EvidenceUnit {
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        let content_hash = format!("{:x}", hasher.finalize());
        // unique id: rollout + content prefix (same content across different
        // sessions must NOT collide on id)
        let mut id_hasher = Sha256::new();
        id_hasher.update(rollout.as_bytes());
        id_hasher.update(content.as_bytes());
        let id_digest = format!("{:x}", id_hasher.finalize());
        let id = format!(
            "ev_test_{}",
            &id_digest[..8]
        );
        let eu = EvidenceUnit {
            id,
            rollout_id: rollout.into(),
            path: "__test__".into(),
            section: section.into(),
            scope: MemoryScope::Workspace,
            status: EvidenceStatus::Active,
            content: content.into(),
            content_hash,
            occurred_at: Utc::now(),
            created_at: Utc::now(),
            superseded_by: None,
            superseded_at: None,
            rollout_available: true,
            rollout_expired_at: None,
            subchunk_index: 0,
        };
        db.upsert_evidence_unit(&eu).unwrap();
        eu
    }

    #[test]
    fn consolidation_promotes_three_identical_evidences() {
        let db = make_db();
        let e1 = insert_evidence(&db, "[exec] missing openssl libssl.so.3", "r1", "tool_result");
        let e2 = insert_evidence(&db, "[exec] missing openssl libssl.so.3", "r2", "tool_result");
        let _e3 = insert_evidence(&db, "[exec] missing openssl libssl.so.3", "r3", "tool_result");
        assert_ne!(e1.id, e2.id, "different rollouts should yield different evidence ids");

        let rpt = db.run_consolidation_pass().unwrap();
        assert_eq!(rpt.memories_created, 1);
        assert_eq!(rpt.evidence_superseded, 3);
        // Evidence should now be superseded
        let got = db.get_evidence_unit(&e1.id).unwrap().unwrap();
        assert_eq!(got.status, EvidenceStatus::Superseded);
    }

    #[test]
    fn consolidation_skips_single_evidence_below_min() {
        // P1(W3) 之后 MIN_OCCURRENCES=2，所以只剩 1 条 evidence 的组不会升 memory。
        let db = make_db();
        insert_evidence(&db, "single content", "r1", "用户问题");

        let rpt = db.run_consolidation_pass().unwrap();
        assert_eq!(rpt.memories_created, 0);
        assert_eq!(rpt.groups_evaluated, 1);
        assert_eq!(rpt.groups_insufficient, 1);
    }

    #[test]
    fn consolidation_promotes_two_identical_evidences_after_min_drop() {
        // P1(W3) MIN_OCCURRENCES 从 3 → 2：之前这个用例断言不 promote，
        // 现在断言一定会 promote，以守住新阈值语义。
        let db = make_db();
        insert_evidence(&db, "same content", "r1", "用户问题");
        insert_evidence(&db, "same content", "r2", "用户问题");

        let rpt = db.run_consolidation_pass().unwrap();
        assert_eq!(rpt.memories_created, 1);
        assert_eq!(rpt.evidence_superseded, 2);
    }

    #[test]
    fn consolidation_is_idempotent() {
        let db = make_db();
        insert_evidence(&db, "重复三次", "r1", "用户问题");
        insert_evidence(&db, "重复三次", "r2", "用户问题");
        insert_evidence(&db, "重复三次", "r3", "用户问题");

        let r1 = db.run_consolidation_pass().unwrap();
        let r2 = db.run_consolidation_pass().unwrap();
        assert_eq!(r1.memories_created, 1);
        assert_eq!(r2.memories_created, 0, "idempotent second pass must not re-promote");
    }
}
