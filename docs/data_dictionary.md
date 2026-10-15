# データ辞書・スキーマ定義 (Data Dictionary & Schema Reference)

本ドキュメントでは、`scm-lp-optimizer` の入力 JSON (`OptimizeRequestPayload`) および出力 JSON (`OptimizeResponsePayload`)、ならびに `templates/csv/` の各テーブルの項目仕様を定義します。

---

## 1. 入力ルート構造 (`OptimizeRequestPayload`)

| フィールド名 | 型 | 必須 | 説明 |
| :--- | :--- | :---: | :--- |
| `config` | `SolverConfig` | 任意 | コスト係数・ペナルティ・求解モードの設定（省略時はデフォルト値適用） |
| `facts` | ` Vec<FactRow>` | **必須** | 拠点×品目×週ごとの需要・現行計画トランザクション |
| `lines` | `Vec<ProductionLineRow>` | 任意 | 製造ラインおよび能力マスタ |
| `bom` | `Vec<BomRow>` | 任意 | 部品表（BOM）・製造速度・歩留まり・スクラップ率マスタ |
| `transport_routes` | `Vec<TransportRouteRow>` | 任意 | 拠点間輸送ルート・輸送リードタイム・輸送費・クォータマスタ |
| `item_locations` | `Vec<ItemLocationRow>` | 任意 | 拠点×品目別のロットサイズ・リードタイム・単価・原価・安全在庫週数マスタ |
| `locations` | `Vec<LocationRow>` | 任意 | 拠点別の保管費・保管能力マスタ |
| `init_inventory` | `Vec<InitInventoryRow>` | 任意 | 計画開始時点（$t=0$ 期首）の初期在庫データ |
| `scheduled_receipts` | `Vec<ScheduledReceiptRow>` | 任意 | すでに発注済みで計画期間中に着荷する確定入荷予定データ |
| `capacity_overrides` | `Vec<CapacityOverrideRow>` | 任意 | 特定週のライン能力オーバーライド（休日稼働・定期修繕など） |
| `backorder_skus` | `Vec<String>` | 任意 | 品切れ時に翌週へバックオーダー（受注残）として繰り越す対象 SKU のリスト |
| `request_map` | `Vec<RequestMapEntry>` | 任意 | 無制約MRPパス（Pass-1）の要求量マップ（`production_request` / `replenishment_request` 記録用） |
| `request_kind_map` | `Vec<RequestKindEntry>` | 任意 | 各拠点×品目の要求種別（`"production_qty"` または `"replenishment_qty"`） |

---

## 2. ソルバー設定 (`config`: `SolverConfig`)

| フィールド名 | 型 | デフォルト値 | 説明 |
| :--- | :--- | :---: | :--- |
| `holding_cost` | `f64` | `1.0` | デフォルトの在庫保管コスト（1単位・1週間あたり） |
| `shortage_penalty` | `f64` | `1000.0` | 需要未充足（品切れ）1単位あたりのペナルティコスト |
| `procurement_cost` | `f64` | `0.1` | 外部サプライヤーからのデフォルト調達コスト（1単位あたり） |
| `production_cost` | `f64` | `5.0` | デフォルトの製造コスト（1単位あたり） |
| `replenishment_cost` | `f64` | `2.0` | デフォルトの拠点間輸送補充コスト（1単位あたり） |
| `ss_violation_penalty` | `f64` | `100.0` | 安全在庫ターゲット未達1単位・1週間あたりのペナルティコスト |
| `backlog_penalty_per_week` | `f64` | `50.0` | `backorder_skus` 対象品目のバックオーダー繰り越し1週間あたりの追加ペナルティ |
| `plan_stability_lambda` | `f64` | `0.0` | 前回計画からの変動抑制ペナルティ係数 $\lambda$（`> 0` で有効化） |
| `profit_maximization` | `bool` | `false` | `true` の場合、品切れペナルティを品目売価 `price` と一致させ利益最大化として求解 |
| `terminal_lookback_weeks` | `usize?` | `4` | 計画期間末尾の安全在庫計算時に外挿する直近需要の平均参照週数 |
| `formulation_mode` | `String` | `"scipy"` | `"scipy"`（輸送エッジ変数 `TR` を展開）または `"glop"`（補充変数 `R` に直接クォータ係数を適用） |

---

## 3. トランザクション・マスタ各テーブル定義

### 3.1 週次計画ファクト (`facts` / `facts.csv`)
計画対象となるすべての `(location, sku, week_start)` の組み合わせを格納します。`week_start` は `YYYY-MM-DD` 形式で、すべての週が7日間隔である必要があります。

| フィールド名 | 型 | 必須 | デフォルト | 説明 |
| :--- | :--- | :---: | :---: | :--- |
| `location` | `String` | **必須** | - | 拠点コード（例: `"PLANT-A"`, `"DC-EAST"`） |
| `sku` | `String` | **必須** | - | 品目コード（例: `"FG-1"`, `"RAW-1"`） |
| `week_start` | `String` | **必須** | - | 週開始日（`YYYY-MM-DD` 形式、7日等間隔） |
| `sales_qty` | `f64` | 任意 | `0.0` | 当該拠点・品目・週の顧客販売需要（独立需要） |
| `dependent_demand_qty` | `f64` | 任意 | `0.0` | 従属需要（参考保持値。LP内部ではBOMと輸送ルートから動的に拘束されます） |
| `production_qty` | `i64` | 任意 | `0` | 現行（前回）の計画生産量。フリーズフェンス内固定値や計画安定化ペナルティの基準値として使用され、最適化後に上書き返却されます |
| `replenishment_qty` | `i64` | 任意 | `0` | 現行（前回）の計画補充入荷量。最適化後に上書き返却されます |
| `order_release_qty` | `i64?` | 任意 | `null` | 外部サプライヤーへの購買発注量（最適化後に上書き返却されます） |
| `safety_stock_weeks` | `f64?` | 任意 | `null` | `item_locations.safety_stock_weeks` が未設定の場合のフォールバック安全在庫週数 |
| `production_fence_weeks` | `i64?` | 任意 | `0` | 生産計画を現行値に固定するフリーズフェンス週数（先頭からの週数） |
| `replenishment_fence_weeks` | `i64?` | 任意 | `0` | 補充計画を現行値に固定するフリーズフェンス週数（先頭からの週数） |

> **補足**: `FactRow` には `#[serde(flatten)]` が設定されているため、上記以外のカスタム列（例: `version_id`, `beginning_inventory` 等）を入力 JSON に含めた場合も、欠落せずそのまま出力 `facts` に保持（ラウンドトリップ）されます。

---

### 3.2 製造ラインマスタ (`lines` / `lines.csv`)

| フィールド名 | 型 | 必須 | デフォルト | 説明 |
| :--- | :--- | :---: | :---: | :--- |
| `location` | `String` | **必須** | - | 工場拠点コード |
| `line_id` | `String` | **必須** | - | 製造ラインID |
| `max_capacity` | `f64` | **必須** | - | 週あたりの最大生産数量能力（数量ベース制約時および拠点合計能力の算出に使用） |
| `available_hours_per_week` | `f64?` | 任意 | `120.0` | 週あたりの利用可能稼働時間（時間ベース能力制約時に使用） |

---

### 3.3 部品表マスタ (`bom` / `bom.csv`)

| フィールド名 | 型 | 必須 | デフォルト | 説明 |
| :--- | :--- | :---: | :---: | :--- |
| `target_location` | `String` | **必須** | - | 親品目を製造する拠点コード |
| `parent_sku` | `String` | **必須** | - | 親品目（完成品・半製品）コード |
| `child_sku` | `String` | **必須** | - | 子品目（原材料・部品）コード |
| `quantity` | `f64` | **必須** | - | 親品目1単位あたりの子品目基準所要量 |
| `line_id` | `String?` | 任意 | `null` | 親品目を製造するラインID（`lines` マスタの `line_id` と紐づけ） |
| `rate_per_hour` | `f64?` | 任意 | `null` | 1時間あたりの親品目製造スピード（設定時は時間ベース能力制約が適用されます） |
| `yield_rate` | `f64?` | 任意 | `1.0` | 製造歩留まり率 $\eta$ $(0 < \eta \le 1.0]$ |
| `scrap_rate` | `f64?` | 任意 | `0.0` | 子部品のスクラップ率 $s$ $[0.0 \le s < 1.0)$ |

---

### 3.4 輸送ルートマスタ (`transport_routes` / `transport_routes.csv`)

| フィールド名 | 型 | 必須 | デフォルト | 説明 |
| :--- | :--- | :---: | :---: | :--- |
| `from_location` | `String` | **必須** | - | 出荷元拠点コード |
| `to_location` | `String` | **必須** | - | 着地拠点コード |
| `sku` | `String` | **必須** | - | 輸送対象の品目コード |
| `transport_lt` | `i64?` | 任意 | `0` | 輸送リードタイム（週単位） |
| `transport_cost_per_unit` | `f64?` | 任意 | `0.0` | 1単位あたりの輸送コスト |
| `min_shipping_qty` | `i64?` | 任意 | `0` | 最小輸送出荷数量（MOQ） |
| `shipping_lot_size` | `i64?` | 任意 | `1` | 輸送出荷ロット刻み単位 |
| `quota_pct` | `f64?` | 任意 | `null` | 同一着地・同一品目へ複数ルートがある場合の固定配分比率（全ルートに設定された場合のみ比率固定モードが有効化） |

---

### 3.5 品目×拠点パラメータマスタ (`item_locations` / `item_locations.csv`)

| フィールド名 | 型 | 必須 | デフォルト | 説明 |
| :--- | :--- | :---: | :---: | :--- |
| `location` | `String` | **必須** | - | 拠点コード |
| `sku` | `String` | **必須** | - | 品目コード |
| `can_procure` | `bool?` | 任意 | 自動判定 | 外部サプライヤーからの購買可否オーバーライド |
| `production_lt` | `i64?` | 任意 | `0` | 製造リードタイム（週単位） |
| `safety_lt_weeks` | `i64?` | 任意 | `0` | 安全リードタイム週数（製造開始ブロック期間に加算） |
| `min_production_qty` | `i64?` | 任意 | `0` | 最小生産数量（製造MOQ） |
| `production_lot_size` | `i64?` | 任意 | `1` | 生産ロット刻み単位 |
| `min_procurement_qty` | `i64?` | 任意 | `0` | 最小購買発注数量（購買MOQ） |
| `purchase_lot_size` | `i64?` | 任意 | `1` | 購買発注ロット刻み単位 |
| `weekly_procurement_capacity` | `f64?` | 任意 | 無制限 | 週あたりの外部購買発注上限数量 |
| `procurement_lt_weeks` | `i64?` | 任意 | `0` | 外部購買リードタイム（週単位） |
| `safety_stock_weeks` | `f64?` | 任意 | `0.0` | 目標安全在庫週数（先行何週分の需要を在庫として持つか） |
| `excess_inventory_buffer_ratio` | `f64?` | 任意 | `1.2` | 過剰在庫クリッピング（L-10）の上限倍率 |
| `price` | `f64?` | 任意 | `null` | 販売単価（品切れペナルティ下限、利益最大化モード、および能力クリップ優先順位に使用） |
| `cost` | `f64?` | 任意 | `null` | 標準原価（製造コストおよび保管費率計算に使用） |
| `procurement_unit_cost` | `f64?` | 任意 | `null` | 外部購買単価 |

---

### 3.6 拠点マスタ (`locations` / `locations.csv`)

| フィールド名 | 型 | 必須 | デフォルト | 説明 |
| :--- | :--- | :---: | :---: | :--- |
| `location` | `String` | **必須** | - | 拠点コード |
| `holding_cost` | `f64?` | 任意 | `config.holding_cost` | 拠点固有の在庫保管コスト（1単位・1週あたり） |
| `holding_cost_rate` | `f64?` | 任意 | `null` | 原価比例の在庫保管費率（`cost` に対する週次比率） |
| `max_storage_capacity` | `f64?` | 任意 | 無制限 | 拠点全体の最大保管在庫数量（全SKU合計） |

---

### 3.7 期首在庫 (`init_inventory` / `init_inventory.csv`)

| フィールド名 | 型 | 必須 | 説明 |
| :--- | :--- | :---: | :--- |
| `location` | `String` | **必須** | 拠点コード |
| `sku` | `String` | **必須** | 品目コード |
| `qty` | `f64` | **必須** | $t=0$ 期首時点の利用可能在庫数量 |

---

### 3.8 確定入荷予定 (`scheduled_receipts` / `scheduled_receipts.csv`)

| フィールド名 | 型 | 必須 | 説明 |
| :--- | :--- | :---: | :--- |
| `location` | `String` | **必須** | 着荷拠点コード |
| `sku` | `String` | **必須** | 品目コード |
| `due_week` | `String` | **必須** | 入荷予定日（`YYYY-MM-DD`。該当する週次バケットへ自動集計されます） |
| `qty` | `f64` | **必須** | 確定入荷数量 |

---

### 3.9 週別ライン能力オーバーライド (`capacity_overrides` / `capacity_overrides.csv`)

| フィールド名 | 型 | 必須 | 説明 |
| :--- | :--- | :---: | :--- |
| `location` | `String` | **必須** | 拠点コード |
| `line_id` | `String` | **必須** | ラインID |
| `week_start` | `String` | **必須** | 対象週（`YYYY-MM-DD`） |
| `max_capacity` | `f64` | **必須** | 当該週のオーバーライド最大能力（稼働時間も比率に応じて自動スケーリングされます） |

---

## 4. 出力ルート構造 (`OptimizeResponsePayload`)

| フィールド名 | 型 | 説明 |
| :--- | :--- | :--- |
| `status` | `String` | `"optimal"`（最適解到達） |
| `elapsed_ms` | `u64` | 計算所要時間（ミリ秒） |
| `facts` | `Vec<FactRow>` | 最適化された `production_qty`, `replenishment_qty`, `order_release_qty`, `production_request`, `replenishment_request` が書き戻された全ファクト行 |
| `edits` | `Vec<PlanEditRow>` | 入力時点の値から変化があったセルのみを抽出した差分リスト（DBへの差分UPDATE用） |
| `bottlenecks` | `Vec<Bottleneck>` | 能力制約がバインド（上限到達）しているライン・拠点・倉庫のシャドウプライス一覧 |
