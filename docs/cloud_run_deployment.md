# Google Cloud Run デプロイ・外部システム連携ガイド

`scm-lp-optimizer` はステートレスな HTTP マイクロサービス (`scm-lp-server`) として動作し、Google Cloud Run 上でオートスケール環境として運用できます。

---

## 1. ローカルでの Docker ビルドと動作確認

```bash
# コンテナイメージのビルド
docker build -t scm-lp-optimizer:latest .

# ローカルで起動 (ポート 8080)
docker run --rm -p 8080:8080 -e PORT=8080 scm-lp-optimizer:latest
```

動作確認：

```bash
curl http://localhost:8080/health
curl -X POST http://localhost:8080/optimize \
  -H "Content-Type: application/json" \
  -d @examples/basic_2echelon_request.json
```

---

## 2. Google Cloud Run へのデプロイ手順

Google Cloud SDK (`gcloud`) を使用して、ソースコードから直接 Cloud Run へデプロイできます。

```bash
# 変数設定
export PROJECT_ID="your-gcp-project-id"
export REGION="asia-northeast1"
export SERVICE_NAME="scm-lp-optimizer"

# Cloud Build + Cloud Run デプロイ
gcloud run deploy "${SERVICE_NAME}" \
  --project="${PROJECT_ID}" \
  --region="${REGION}" \
  --source=. \
  --memory=2Gi \
  --cpu=2 \
  --timeout=300s \
  --concurrency=4 \
  --allow-unauthenticated
```

> **本番運用時の推奨設定**:
> - 認証を必須にする場合は `--allow-unauthenticated` の代わりに `--no-allow-unauthenticated` を指定し、呼び出し元（Python バックエンド等）のサービスアカウントに `roles/run.invoker` 権限を付与してください。
> - 大規模な多期間モデルを同時に複数リクエスト処理する場合は、`--cpu=4 --memory=4Gi` へのスケールアップを推奨します。

---

## 3. Python からの呼び出し実装例

### 3.1 REST API (`scm-lp-server` / Cloud Run) を呼び出す場合

```python
import json
from pathlib import Path
import urllib.request

SERVICE_URL = "http://localhost:8080/optimize"  # または Cloud Run の URL

payload_path = Path("examples/basic_2echelon_request.json")
req_bytes = payload_path.read_bytes()

req = urllib.request.Request(
    SERVICE_URL,
    data=req_bytes,
    headers={"Content-Type": "application/json"},
    method="POST",
)

with urllib.request.urlopen(req, timeout=60) as resp:
    result = json.loads(resp.read().decode("utf-8"))

print(f"Status: {result['status']}, Elapsed: {result['elapsed_ms']} ms")
print(f"Changed cells (edits): {len(result['edits'])}")
for b in result["bottlenecks"]:
    print(f"Bottleneck: {b['description']} (marginal={b['marginal_value']})")
```

### 3.2 サブプロセスとして CLI (`scm-lp-cli`) を呼び出す場合

同一コンテナ内やローカルバッチ処理から直接バイナリを呼び出す場合は、ネットワークオーバーヘッドなしで標準入出力経由の連携が可能です：

```python
import json
import subprocess
from pathlib import Path

payload = json.loads(Path("examples/basic_2echelon_request.json").read_text(encoding="utf-8"))

proc = subprocess.run(
    ["./target/release/scm-lp-cli"],
    input=json.dumps(payload).encode("utf-8"),
    capture_output=True,
    check=True,
)

result = json.loads(proc.stdout.decode("utf-8"))
print(f"Status: {result['status']}, Edits count: {len(result['edits'])}")
```
