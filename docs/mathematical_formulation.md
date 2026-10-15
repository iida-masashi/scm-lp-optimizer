# 数理最適化モデル・アルゴリズム解説 (Mathematical Formulation)

本ドキュメントでは、`scm-lp-optimizer` が内部で構築・求解する**マルチ期間・能力制約付き線形計画（Multi-Period Capacitated LP）** の数理定式化およびポスト処理パイプラインを解説します。

---

## 1. インデックスと集合 (Sets & Indices)

- $p \in \{0, \dots, N-1\}$: 計画対象となる **拠点×品目ペア** `(location, sku)` の集合（辞書順ソート）。
  - 各ノード $p$ はマスタ定義に基づき、以下の3つの供給能力フラグを持ちます：
    - `can_produce` ($\text{canProduce}_p \in \{\text{true}, \text{false}\}$): 当該拠点に製造能力 (`max_capacity`) があり、かつ親品目として BOM が定義されている場合 `true`
    - `can_replenish` ($\text{canReplenish}_p \in \{\text{true}, \text{false}\}$): 当該拠点・品目を着地 (`to_location`, `sku`) とする輸送ルートが存在する場合 `true`
    - `can_procure` ($\text{canProcure}_p \in \{\text{true}, \text{false}\}$): 外部サプライヤーからの購買調達が可能である場合 `true`（`item_locations.can_procure` で明示指定可能。未指定時は「製造も拠点間補充もされず、どの拠点でも製造品目として登録されていない原材料SKU」が自動的に `true` となります）
- $t \in \{0, \dots, T-1\}$: 週次計画バケット（`week_start` の昇順ソート）。
  - **一様週次軸の検証**: 隣接する `week_start` の間隔がすべて **7日間** であることを求解前に検証します（欠落週や月次混在がある場合はリードタイムのシフトが歪むためエラーを返します）。
- $e \in \mathcal{E}_{\text{tr}}$: 拠点間輸送エッジ $(p_{\text{src}}, p_{\text{dst}}, LT_e, \text{cost}_e)$ の集合。

---

## 2. 事前計算：下流需要伝播と安全在庫ターゲット

### 2.1 下流需要のトポロジカル逆伝播 (`propagate_downstream_demand`)
各ノード $p$ の外生需要（顧客販売需要 `sales_qty`） $D_{p, t} = \max(0, \text{sales}_{p, t})$ に加え、安全在庫ターゲット計算用の実効需要 $\tilde{D}_{p, t}$ を、下流（消費側）から上流（供給側）へトポロジカル順序で逆伝播して算出します：

- **輸送ルート $(p_{\text{src}} \to p_{\text{dst}}, LT_{\text{tr}}, q)$**:
  $$\tilde{D}_{p_{\text{src}}, t} \mathrel{+}= q \cdot \tilde{D}_{p_{\text{dst}}, t + LT_{\text{tr}}}$$
- **BOM $(p_{\text{child}} \to p_{\text{parent}}, \text{qty}_{\text{eff}}, LT_{\text{prod}})$**:
  $$\tilde{D}_{p_{\text{child}}, t} \mathrel{+}= \text{qty}_{\text{eff}} \cdot \tilde{D}_{p_{\text{parent}}, t + LT_{\text{prod}}}$$
  ここで、スクラップ率 $s \in [0, 1)$ を考慮した実効部品所要量は $\text{qty}_{\text{eff}} = \frac{\text{quantity}}{1 - s}$ です。

### 2.2 動的安全性在庫ターゲット (`compute_safety_stock_target`)
各ノード $p$ の安全在庫週数 $W_{p}^{\text{ss}}$（`safety_stock_weeks`）に対し、週 $t$ 末の目標安全在庫量 $SS^{\text{target}}_{p, t}$ は、翌週以降の先行 $W_{p}^{\text{ss}}$ 週分の実効需要の合計として算出されます：

$$SS^{\text{target}}_{p, t} = \sum_{j=1}^{\lfloor W_p^{\text{ss}} \rfloor} \tilde{D}_{p, t+j} + \left(W_p^{\text{ss}} - \lfloor W_p^{\text{ss}} \rfloor\right) \tilde{D}_{p, t + \lfloor W_p^{\text{ss}} \rfloor + 1}$$

※計画ホライズン末尾（$t + j \ge T$）を超える期間については、直近 $K$ 週（デフォルト $K=4$ 週、`terminal_lookback_weeks` で変更可）の平均実効需要を外挿してターゲット維持を図ります。

---

## 3. 決定変数 (Decision Variables)

すべての決定変数は連続非負変数（$\ge 0$）です。

### 3.1 基本変数（各ペア $p$・各週 $t$ ごとに 6 スロット）
1. $P_{p, t}$ (`SLOT_P`): 週 $t$ に完成・入庫する**計画生産量**（製造リードタイム $LT^{\text{prod}}_p$ 週前に着手し、週 $t$ に利用可能となる量）
2. $R_{p, t}$ (`SLOT_R`): 週 $t$ に着荷・入庫する**計画拠点間補充量**（輸送リードタイム $LT^{\text{tr}}$ 週前に出荷元から発送され、週 $t$ に着地へ届く量）
3. $I_{p, t}$ (`SLOT_I`): 週 $t$ 末の**計画期末在庫量**
4. $U_{p, t}$ (`SLOT_U`): 週 $t$ の**未充足需要量（品切れ / バックオーダー量）**
5. $E_{p, t}$ (`SLOT_E`): 週 $t$ に外部サプライヤーへ発注する**計画購買発注量（Order Release）**（購買リードタイム $LT^{\text{proc}}_p$ 週後の週 $t + LT^{\text{proc}}_p$ に着荷）
6. $SS_{p, t}$ (`SLOT_SS`): 週 $t$ 末の**安全在庫目標未達スラック量（Safety Stock Violation）**

### 3.2 拡張変数
7. $TR_{e, t}$: **ネットワーク設計用・輸送エッジ別流量変数**
   - クォータ比率固定（`quota_pct`）が使われていない場合（かつ `formulation_mode = "scipy"` の場合）、複数供給元からの最適ルート選択を可能にするため、各輸送ルート $e = (p_{\text{src}} \to p_{\text{dst}})$ ごとに週 $t$（着地到着週基準）の輸送量 $TR_{e, t} \ge 0$ を導入します。
8. $D^P_{p, t}, D^R_{p, t}$: **計画安定化（Plan Stability）L1偏差変数**
   - `plan_stability_lambda` ($\lambda > 0$) が設定されている場合、前回計画値（入力 `facts` の `production_qty`, `replenishment_qty`）からの絶対変動量 $|P_{p, t} - P^{\text{prev}}_{p, t}|$, $|R_{p, t} - R^{\text{prev}}_{p, t}|$ を表す補助変数です。

---

## 4. 目的関数 (Objective Function)

総コスト最小化問題として以下の線形目的関数 $\min c^T x$ を最小化します：

$$\min \sum_{p=0}^{N-1} \sum_{t=0}^{T-1} \left( c^P_{p, t} P_{p, t} + c^R_{p, t} R_{p, t} + c^E_{p, t} E_{p, t} + c^I_{p, t} I_{p, t} + c^U_{p, t} U_{p, t} + c^{SS}_{p, t} SS_{p, t} \right) + \sum_{e \in \mathcal{E}_{\text{tr}}} \sum_{t=0}^{T-1} c^{\text{tr}}_e TR_{e, t} + \lambda \sum_{p, t} \left( D^P_{p, t} + D^R_{p, t} \right)$$

各係数の決定ルールは以下の通りです：
- **在庫保管コスト $c^I_{p, t}$**:
  - 拠点に原価比例保管費率 `holding_cost_rate` ($r^{\text{hold}}_{\text{loc}}$) があり、品目に原価 `cost` ($\text{cost}_p$) がある場合は $r^{\text{hold}}_{\text{loc}} \times \text{cost}_p$。
  - それ以外は拠点の `holding_cost`、未指定時は `config.holding_cost`（デフォルト `1.0`）。
- **生産コスト $c^P_{p, t}$**: 品目原価 `item_locations.cost`、未指定時は `config.production_cost`（デフォルト `5.0`）。
- **外部調達コスト $c^E_{p, t}$**: `item_locations.procurement_unit_cost` ➔ `item_locations.cost` ➔ `config.procurement_cost`（デフォルト `0.1`）の優先順。
- **輸送補充コスト $c^R_{p, t}$ / $c^{\text{tr}}_e$**: ルートの `transport_cost_per_unit`、未指定時は `config.replenishment_cost`（デフォルト `2.0`）。
- **品切れペナルティ $c^U_{p, t}$**:
  - 通常モード（`profit_maximization = false`）: $\max(\text{price}_p, w_{\text{short}})$（ただし $w_{\text{short}}$ は `config.shortage_penalty`、デフォルト `1000.0`）。
  - 利益最大化モード（`profit_maximization = true`）: 品目単価 `price` が設定されている場合、品切れの機会損失コストをちょうど `price` とすることで、「限界利益（売価 − 原価 − 輸送・保管費）が正の需要のみを優先充足する」利益最大化と同値な最適化を行います。
  - **バックオーダー品目 (`backorder_skus`)**: 繰り越し1週間あたり `config.backlog_penalty_per_week`（デフォルト `50.0`）が加算され、さらにホライズン最終週 $t = T-1$ の未解消バックオーダーには `shortage_cost` が課されます。
- **安全在庫割れペナルティ $c^{SS}_{p, t}$**: `config.ss_violation_penalty`（デフォルト `100.0`）。

---

## 5. 制約条件 (Constraints)

### 5.1 在庫バランス等式制約 ($A_{\text{eq}} x = b_{\text{eq}}$)
各ノード $p$・各週 $t$ について、以下の物質保存則（Material Balance）が成立します：

$$I_{p, t} - I_{p, t-1} - P_{p, t} - R_{p, t} - E_{p, t - LT^{\text{proc}}_p} - U^{\text{net}}_{p, t} + \text{BOM消費}_{p, t} + \text{輸送出荷}_{p, t} = \text{SR}_{p, t} - D_{p, t} + \delta_{t=0} I^{\text{init}}_p$$

- **初期在庫 $I^{\text{init}}_p$**: $t=0$ において右辺に加算されます（$t=0$ では $I_{p, -1} = 0$）。
- **確定入荷予定 $\text{SR}_{p, t}$**: `scheduled_receipts` を週バケットに集計した確定入荷量です。
- **未充足需要項 $U^{\text{net}}_{p, t}$**:
  - 通常（Lost Sales）品目: $U^{\text{net}}_{p, t} = U_{p, t}$
  - バックオーダー（Backorder）品目: $U^{\text{net}}_{p, t} = U_{p, t} - U_{p, t-1}$（前期の未充足需要が当期の要求として繰り越されます）
- **BOM消費 $\text{BOM消費}_{p, t}$**:
  - 子部品 $p$ を消費する親品目 $p_{\text{parent}}$（製造リードタイム $LT^{\text{prod}}$、実効所要量 $q_{\text{eff}}$）に対し、親が週 $t + LT^{\text{prod}}$ に完成させる生産量 $P_{p_{\text{parent}}, t + LT^{\text{prod}}}$ のために週 $t$ に引き落とされます。
  - ※ $t=0$ においては、リードタイム内の初期期間 $t' \in \{0, \dots, LT^{\text{prod}}\}$ の生産分がまとめて $t=0$ の在庫から引き落とされます。
- **輸送出荷 $\text{輸送出荷}_{p, t}$**:
  - 供給元 $p$ から着地 $p_{\text{dst}}$（輸送リードタイム $LT^{\text{tr}}$）への発送量は、着地に週 $t + LT^{\text{tr}}$ に到着する補充量 $R_{p_{\text{dst}}, t + LT^{\text{tr}}}$（またはエッジ変数 $TR_{e, t + LT^{\text{tr}}}$）として週 $t$ に引き落とされます。

### 5.2 製造ライン能力制約 ($A_{\text{ub}} x \le b_{\text{ub}}$)
拠点・製造ライン `(loc, line)` に紐づく品目集合 $P_{\text{line}}$ について、週 $t$ の生産量はライン能力上限を超えません：

- **時間ベース能力制約**（ラインの稼働可能時間 `available_hours_per_week` $> 0$ かつ全割当品目に生産レート `rate_per_hour` ($r_p$) および歩留まり `yield_rate` ($\eta_p$) が定義されている場合）:
  $$\sum_{p \in P_{\text{line}}} \frac{1}{r_p \times \eta_p} P_{p, t} \le \text{Hours}_{\text{loc}, \text{line}, t}$$
- **数量ベース能力制約**（上記以外の場合）:
  $$\sum_{p \in P_{\text{line}}} P_{p, t} \le \text{Cap}_{\text{loc}, \text{line}, t}$$

### 5.3 拠点全体能力制約・保管倉庫能力制約
- 特定ラインに紐づかない製造品目については、拠点能力 $\text{LocCap}_{\text{loc}, t}$ を上限とします。
- `locations.max_storage_capacity` が設定されている拠点では、当該拠点の全品目の期末在庫合計が保管能力を超えません：
  $$\sum_{p \in P_{\text{loc}}} I_{p, t} \le \text{StorageCap}_{\text{loc}}$$

### 5.4 安全在庫フロア制約
$$I_{p, t} + SS_{p, t} \ge SS^{\text{target}}_{p, t} \quad \Longleftrightarrow \quad -I_{p, t} - SS_{p, t} \le -SS^{\text{target}}_{p, t}$$

### 5.5 フリーズフェンス（固定期間）と境界条件
- `production_fence_weeks` ($F^P$) 内の週 $t < F^P$ では、$P_{p, t} = P^{\text{prev}}_{p, t}$ に固定されます。
- `replenishment_fence_weeks` ($F^R$) 内の週 $t < F^R$ では、$R_{p, t} = R^{\text{prev}}_{p, t}$ に固定されます。
- リードタイム未満の初期週（$t < LT^{\text{prod}}_p$ または $t < LT^{\text{tr}}_p$）でフェンス外の場合は、新規手配が間に合わないため $P_{p, t} = 0$（または $R_{p, t} = 0$）に固定されます。

---

## 6. シャドウプライス（潜在価格 / ボトルネック抽出）

Clarabel の主双対内点法により得られた双対変数（Dual Variables）から、ライン能力・拠点能力・保管能力の各不等式制約に対する限界価値（Shadow Price = 右辺の能力を1単位増やしたときの目的関数改善額、`marginal_value`: $\pi$）を抽出します。
- $|\pi| > 10^{-4}$ となる制約を、アクティブなボトルネック（`bottlenecks`）として出力します。

---

## 7. ポスト処理パイプライン (Post-Processing Pipeline)

連続LPの最適解 $x^*$ を実務の離散ロット制約に適合させるため、以下の4段階の決定論的ポスト処理を実行します：

1. **ロットサイズ丸め (`round_lp_solution`)**:
   - 各品目・拠点の最小ロット（MOQ: `min_production_qty`, `min_procurement_qty`, `min_shipping_qty`）および刻みロット（`production_lot_size`, `purchase_lot_size`, `shipping_lot_size`）に従い、連続解を切り上げ丸め（Ceiling）します。
2. **能力上限クリッピング (`clip_by_capacity`)**:
   - ロット切り上げによってライン能力または拠点能力を超過した場合、利益マージン（`price - cost`）が高い品目を優先して能力枠を割り当て、超過分をロット単位で切り詰めます。
3. **PMU-9b 上流供給連動クリッピング (`clip_transport_to_supply`)**:
   - 上流拠点での能力クリッピング等の結果、上流から出荷可能な実在庫よりも下流の計画補充量 $R$ が大きくなった場合、トポロジカル順序（上流 ➔ 下流）で実際に供給可能な数量以下に下流の入荷量をクリップします。
4. **L-10 過剰在庫クリッピング (`clip_excess_inventory`)**:
   - ロット丸め等の影響で期末在庫が安全在庫ターゲットの一定倍率（`excess_inventory_buffer_ratio`、デフォルト `1.2` 倍）を超える場合、欠品を起こさない範囲で生産ロットを1ロットずつ削減します。
