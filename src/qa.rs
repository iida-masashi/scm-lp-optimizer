use std::collections::BTreeSet;
use std::process::Command;

use crate::run_optimization;
use crate::schema::{
    AskRequestPayload, AskResponsePayload, OptimizeResponsePayload, OptimizerError,
    RootCauseDiagnostic, WhatIfAction,
};

/// Process a natural-language question about the optimization input & output.
pub fn answer_question(payload: &AskRequestPayload) -> Result<AskResponsePayload, OptimizerError> {
    let resp: OptimizeResponsePayload = match &payload.response {
        Some(r) => r.clone(),
        None => run_optimization(&payload.request)?,
    };

    let q_raw = payload.question.trim();
    let q_lower = q_raw.to_lowercase();

    // Detect mentioned locations, SKUs, lines, or weeks in the question
    let mut all_locs: BTreeSet<String> = BTreeSet::new();
    let mut all_skus: BTreeSet<String> = BTreeSet::new();
    let mut all_lines: BTreeSet<String> = BTreeSet::new();
    for r in &resp.sku_psi_details {
        all_locs.insert(r.location.clone());
        all_skus.insert(r.sku.clone());
    }
    for l in &resp.line_utilizations {
        all_lines.insert(l.line_id.clone());
    }

    let matched_locs: Vec<String> = all_locs
        .iter()
        .filter(|loc| q_lower.contains(&loc.to_lowercase()))
        .cloned()
        .collect();
    let matched_skus: Vec<String> = all_skus
        .iter()
        .filter(|sku| q_lower.contains(&sku.to_lowercase()))
        .cloned()
        .collect();

    // Filter relevant root-cause diagnostics
    let mut matched_diagnostics: Vec<RootCauseDiagnostic> = resp
        .root_causes
        .iter()
        .filter(|d| {
            let loc_ok = matched_locs.is_empty() || matched_locs.contains(&d.location);
            let sku_ok = matched_skus.is_empty() || matched_skus.contains(&d.sku);
            loc_ok && sku_ok
        })
        .cloned()
        .collect();
    if matched_diagnostics.is_empty() && !resp.root_causes.is_empty() {
        matched_diagnostics = resp.root_causes.clone();
    }

    // Collect deduplicated What-If actions
    let mut suggested_actions: Vec<WhatIfAction> = Vec::new();
    for d in &matched_diagnostics {
        for act in &d.actions {
            if !suggested_actions.iter().any(|a| a.label == act.label) {
                suggested_actions.push(act.clone());
            }
        }
    }

    // Related (location, sku) pairs for quick UI navigation
    let mut related_skus: Vec<(String, String)> = Vec::new();
    for d in &matched_diagnostics {
        let pair = (d.location.clone(), d.sku.clone());
        if !related_skus.contains(&pair) {
            related_skus.push(pair);
        }
    }
    if related_skus.is_empty() {
        for r in &resp.sku_psi_details {
            let loc_ok = matched_locs.is_empty() || matched_locs.contains(&r.location);
            let sku_ok = matched_skus.is_empty() || matched_skus.contains(&r.sku);
            if loc_ok && sku_ok {
                let pair = (r.location.clone(), r.sku.clone());
                if !related_skus.contains(&pair) {
                    related_skus.push(pair);
                }
            }
        }
    }

    // Build deterministic fact-based analysis in Markdown
    let deterministic_md = build_deterministic_markdown(
        q_raw,
        &q_lower,
        &resp,
        &matched_locs,
        &matched_skus,
        &matched_diagnostics,
        &suggested_actions,
    );

    // Check if Gemini API key is configured for optional LLM synthesis
    let api_key = payload
        .gemini_api_key
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("GEMINI_API_KEY")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        });

    if let Some(key) = api_key {
        if let Some(llm_answer) = call_gemini_with_context(&key, q_raw, &deterministic_md) {
            return Ok(AskResponsePayload {
                engine: "hybrid-gemini-causal-trace".to_string(),
                answer_markdown: llm_answer,
                matched_diagnostics,
                suggested_actions,
                related_skus,
            });
        }
    }

    Ok(AskResponsePayload {
        engine: "deterministic-causal-trace".to_string(),
        answer_markdown: deterministic_md,
        matched_diagnostics,
        suggested_actions,
        related_skus,
    })
}

fn build_deterministic_markdown(
    q_raw: &str,
    q_lower: &str,
    resp: &OptimizeResponsePayload,
    matched_locs: &[String],
    matched_skus: &[String],
    matched_diagnostics: &[RootCauseDiagnostic],
    suggested_actions: &[WhatIfAction],
) -> String {
    let asks_shortage = q_lower.contains("欠品")
        || q_lower.contains("なぜ")
        || q_lower.contains("理由")
        || q_lower.contains("原因")
        || q_lower.contains("不足")
        || q_lower.contains("安全在庫")
        || q_lower.contains("why")
        || q_lower.contains("shortage");

    let asks_line = q_lower.contains("稼働")
        || q_lower.contains("ライン")
        || q_lower.contains("設備")
        || q_lower.contains("ボトルネック")
        || q_lower.contains("能力")
        || q_lower.contains("shadow")
        || q_lower.contains("残業");

    let asks_transport = q_lower.contains("輸送")
        || q_lower.contains("移動")
        || q_lower.contains("出荷")
        || q_lower.contains("ルート")
        || q_lower.contains("補充")
        || q_lower.contains("transport");

    let asks_procurement = q_lower.contains("発注")
        || q_lower.contains("調達")
        || q_lower.contains("購買")
        || q_lower.contains("サプライヤ")
        || q_lower.contains("原材料")
        || q_lower.contains("order");

    let asks_action = q_lower.contains("どうすれ")
        || q_lower.contains("対策")
        || q_lower.contains("解消")
        || q_lower.contains("改善")
        || q_lower.contains("打ち手")
        || q_lower.contains("解決");

    let mut sections: Vec<String> = Vec::new();

    // Total KPIs
    let total_sales: f64 = resp
        .sku_psi_details
        .iter()
        .map(|r| r.sales_demand_qty)
        .sum();
    let total_shortage: f64 = resp.sku_psi_details.iter().map(|r| r.shortage_qty).sum();
    let total_ss_def: f64 = resp
        .sku_psi_details
        .iter()
        .map(|r| r.safety_stock_deficit)
        .sum();
    let fill_rate = if total_sales > 0.0 {
        ((total_sales - total_shortage).max(0.0) / total_sales) * 100.0
    } else {
        100.0
    };

    // 1. Shortage & Root Cause Analysis section
    if asks_shortage || asks_action || (!asks_line && !asks_transport && !asks_procurement) {
        let shortages: Vec<&RootCauseDiagnostic> = matched_diagnostics
            .iter()
            .filter(|d| d.issue_type == "shortage")
            .collect();
        let ss_violations: Vec<&RootCauseDiagnostic> = matched_diagnostics
            .iter()
            .filter(|d| d.issue_type == "safety_stock_deficit")
            .collect();

        if !shortages.is_empty() {
            let mut s = String::new();
            s.push_str(&format!(
                "### 🚨 欠品原因の因果トレース分析（総欠品量: {:.0} 個 / 需要充足率: {:.1}%）\n\n",
                total_shortage, fill_rate
            ));
            s.push_str("サプライチェーンネットワークの上流（販売拠点 ➔ 輸送ルート・リードタイム ➔ 工場製造ライン能力 ➔ BOM原材料調達）を逆探索した結果、以下の**構造的ボトルネック**により欠品が発生しています。\n\n");

            for (idx, d) in shortages.iter().enumerate() {
                s.push_str(&format!(
                    "#### {}. 【{} / {}】 {}週（欠品量: **{:.0} 個**）\n",
                    idx + 1,
                    d.location,
                    d.sku,
                    d.week_start,
                    d.shortage_qty
                ));
                s.push_str(&format!("- **直接の主因**: **{}**\n", d.primary_cause));
                s.push_str("- **上流への因果連鎖（Root-Cause Chain）**:\n");
                for f in &d.factors {
                    s.push_str(&format!(
                        "  - **Step {} [{} / {} ({}週)]**: {}\n",
                        f.step, f.location, f.target_id, f.week_start, f.summary
                    ));
                }
                s.push('\n');
            }
            sections.push(s);
        } else if !ss_violations.is_empty() {
            let mut s = String::new();
            s.push_str(&format!(
                "### ✅ 販売需要の欠品は 0 個（充足率 100.0%）、ただし安全在庫目標割れが {:.0} 個発生\n\n",
                total_ss_def
            ));
            s.push_str("顧客への販売欠品（Unfulfilled Demand）は発生していませんが、以下の拠点・週において期末在庫が**安全在庫目標（Safety Stock Target）**を下回っています。\n\n");
            for (idx, d) in ss_violations.iter().enumerate() {
                s.push_str(&format!(
                    "#### {}. 【{} / {}】 {}週（安全在庫不足: **{:.0} 個**）\n",
                    idx + 1,
                    d.location,
                    d.sku,
                    d.week_start,
                    d.ss_deficit_qty
                ));
                s.push_str(&format!("- **主因**: **{}**\n", d.primary_cause));
                for f in &d.factors {
                    s.push_str(&format!("  - **Step {}**: {}\n", f.step, f.summary));
                }
                s.push('\n');
            }
            sections.push(s);
        } else {
            let mut s = String::new();
            s.push_str("### ✅ 欠品および安全在庫割れは発生していません（需要充足率 100.0%）\n\n");
            s.push_str(&format!(
                "- 計画期間全体の総販売需要 **{:.0} 個** に対し、欠品量は **0 個**、安全在庫目標の未達も **0 個** です。\n",
                total_sales
            ));
            s.push_str("- **欠品を回避できた理由**: 下流拠点の需要タイミングと輸送リードタイムを逆算し、上流工場（製造ライン能力上限内）での先行生産・先行補充輸送、および原材料の事前調達リリースが最適に同期されています。\n");
            sections.push(s);
        }
    }

    // 2. Production Line Utilization & Shadow Price section
    if asks_line || asks_shortage {
        if !resp.line_utilizations.is_empty() {
            let mut s = String::new();
            s.push_str("### 🏭 生産工場・ライン別の週次稼働率と能力ボトルネック\n\n");
            s.push_str("| 工場拠点 | 製造ライン | 週 (`week_start`) | 稼働率 (%) | 使用時間 / 可能時間 (h) | 生産数量 / 上限数量 | ボトルネック判定 | 潜在価格 (Shadow Price) |\n");
            s.push_str("| :--- | :--- | :--- | ---: | ---: | ---: | :---: | ---: |\n");
            for lu in &resp.line_utilizations {
                if !matched_locs.is_empty() && !matched_locs.contains(&lu.location) {
                    continue;
                }
                let badge = if lu.is_bottleneck {
                    "🔴 **上限/律速**"
                } else if lu.utilization_pct >= 80.0 {
                    "🟡 高負荷"
                } else {
                    "🟢 余裕あり"
                };
                s.push_str(&format!(
                    "| `{}` | `{}` | `{}` | **{:.1}%** | {:.1}h / {:.1}h | {} / {:.0} | {} | {:.2} |\n",
                    lu.location,
                    lu.line_id,
                    lu.week_start,
                    lu.utilization_pct,
                    lu.used_hours,
                    lu.available_hours,
                    lu.used_quantity,
                    lu.max_capacity_quantity,
                    badge,
                    lu.shadow_price
                ));
            }
            sections.push(s);
        }
    }

    // 3. Inter-location Transport section
    if asks_transport {
        let mut s = String::new();
        s.push_str("### 🚚 拠点間の週次輸送量（出荷週 ➔ 到着週）\n\n");
        if resp.transport_flows.is_empty() {
            s.push_str("現在、拠点間の輸送ルート実績はありません。\n");
        } else {
            s.push_str("| 発拠点 (`from`) | 着拠点 (`to`) | 品目 (`sku`) | 出荷週 | 到着週 | 輸送LT | 輸送数量 | 輸送コスト |\n");
            s.push_str("| :--- | :--- | :--- | :--- | :--- | ---: | ---: | ---: |\n");
            for tf in &resp.transport_flows {
                s.push_str(&format!(
                    "| `{}` | `{}` | `{}` | `{}` | `{}` | {}週 | **{} 個** | {:.1} |\n",
                    tf.from_location,
                    tf.to_location,
                    tf.sku,
                    tf.ship_week,
                    tf.arrival_week,
                    tf.transport_lt,
                    tf.qty,
                    tf.total_cost
                ));
            }
        }
        sections.push(s);
    }

    // 4. Procurement / Order Release section
    if asks_procurement {
        let proc_rows: Vec<_> = resp
            .sku_psi_details
            .iter()
            .filter(|r| r.can_procure || r.order_release_qty > 0 || r.procurement_in_qty > 0)
            .collect();
        if !proc_rows.is_empty() {
            let mut s = String::new();
            s.push_str("### 📦 外部サプライヤーへの発注量（Order Release）と調達入荷\n\n");
            s.push_str("| 拠点 | 品目 (`sku`) | 週 (`week_start`) | 発注リリース量 (`order_release_qty`) | 調達入荷量 | 確定入荷予定 (`SR`) | BOM従属消費量 | 期末在庫 |\n");
            s.push_str("| :--- | :--- | :--- | ---: | ---: | ---: | ---: | ---: |\n");
            for r in proc_rows {
                s.push_str(&format!(
                    "| `{}` | `{}` | `{}` | **{} 個** | {} 個 | {:.0} 個 | {:.0} 個 | {:.0} 個 |\n",
                    r.location,
                    r.sku,
                    r.week_start,
                    r.order_release_qty,
                    r.procurement_in_qty,
                    r.scheduled_receipt_qty,
                    r.bom_demand_qty,
                    r.ending_inv
                ));
            }
            sections.push(s);
        }
    }

    // 5. Specific SKU / Location PSI table if mentioned in question
    if !matched_skus.is_empty() || !matched_locs.is_empty() {
        let mut s = String::new();
        s.push_str("### 📊 該当拠点・SKUの週次 PSI 明細サマリ\n\n");
        s.push_str("| 拠点 | SKU | 週 | 期首在庫 | 生産 | 輸送入荷 | 調達入荷 | 販売需要 | BOM消費 | 輸送出荷 | 期末在庫 | 安全在庫目標 | 欠品量 |\n");
        s.push_str("| :--- | :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
        for r in &resp.sku_psi_details {
            let loc_ok = matched_locs.is_empty() || matched_locs.contains(&r.location);
            let sku_ok = matched_skus.is_empty() || matched_skus.contains(&r.sku);
            if loc_ok && sku_ok {
                s.push_str(&format!(
                    "| `{}` | `{}` | `{}` | {:.0} | {} | {} | {} | {:.0} | {:.0} | {:.0} | **{:.0}** | {:.0} | **{:.0}** |\n",
                    r.location,
                    r.sku,
                    r.week_start,
                    r.opening_inv,
                    r.production_qty,
                    r.transport_in_qty,
                    r.procurement_in_qty,
                    r.sales_demand_qty,
                    r.bom_demand_qty,
                    r.transport_out_qty,
                    r.ending_inv,
                    r.safety_stock_target,
                    r.shortage_qty
                ));
            }
        }
        sections.push(s);
    }

    // 6. Actionable What-If prescriptions
    if !suggested_actions.is_empty() {
        let mut s = String::new();
        s.push_str("### 💡 欠品・ボトルネック解消に向けた推奨アクション（What-If 処方箋）\n\n");
        for (i, act) in suggested_actions.iter().enumerate() {
            s.push_str(&format!(
                "{}. **{}**\n   - {}\n",
                i + 1,
                act.label,
                act.description
            ));
        }
        s.push_str("\n> 下部の **「ワンクリック適用して再最適化」** ボタンを押すと、上記パラメータを自動反映して即座に解消シミュレーションを実行できます。\n");
        sections.push(s);
    }

    let _ = q_raw;
    sections.join("\n---\n\n")
}

/// Optional Gemini REST API call via system `curl` (zero C/OpenSSL crate build overhead).
/// Falls back cleanly to `None` if `curl` fails or network is unavailable.
fn call_gemini_with_context(api_key: &str, question: &str, deterministic_md: &str) -> Option<String> {
    let prompt = format!(
        "<role>あなたはサプライチェーン計画と数理最適化（LP）に精通したパートナー級経営コンサルタントです。</role>\n\
         <data>\n以下は Rust LP ソルバーが算出した決定論的な因果トレースと PSI・設備稼働率の一次データです：\n{}\n</data>\n\
         <task>ユーザーの質問「{}」に対し、上記の一次データ・数値を正確に引用しながら、原因・背景・具体的な改善アクションを簡潔・構造的な日本語 Markdown で回答してください。</task>\n\
         <constraints>データに存在しない数値を推測・捏造しないこと。</constraints>",
        deterministic_md, question
    );

    let body = serde_json::json!({
        "contents": [{
            "parts": [{ "text": prompt }]
        }],
        "generationConfig": {
            "temperature": 0.2
        }
    });

    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent?key={}",
        api_key
    );

    let output = Command::new("curl")
        .args([
            "-sS",
            "--max-time",
            "12",
            "-H",
            "Content-Type: application/json",
            "-X",
            "POST",
            "-d",
            &body.to_string(),
            &url,
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let json_val: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let text = json_val
        .get("candidates")?
        .get(0)?
        .get("content")?
        .get("parts")?
        .get(0)?
        .get("text")?
        .as_str()?;

    if text.trim().is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}
