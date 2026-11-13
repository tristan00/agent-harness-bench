import hashlib
import json
import sys
import time
from pathlib import Path

import numpy as np
import pandas as pd
import scipy
import sklearn
from sklearn.metrics import accuracy_score
from sklearn.pipeline import make_pipeline
from sklearn.preprocessing import StandardScaler
from sklearn.svm import SVC

SEED = 0


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def load(path):
    df = pd.read_csv(path, header=None)
    if df.shape[1] != 65:
        raise ValueError(f"{path}: expected 65 columns, got {df.shape[1]}")
    X = df.iloc[:, :64].to_numpy(dtype=np.float64)
    y = df.iloc[:, 64].to_numpy(dtype=np.int64)
    return X, y


def main():
    start = time.perf_counter()
    np.random.seed(SEED)
    data_dir = Path(sys.argv[1])
    train_path = data_dir / "optdigits.tra"
    test_path = data_dir / "optdigits.tes"

    X_train, y_train = load(train_path)
    X_test, y_test = load(test_path)

    model = make_pipeline(StandardScaler(), SVC(kernel="rbf", C=1.0, gamma="scale", random_state=SEED))
    model.fit(X_train, y_train)
    reference_score = float(accuracy_score(y_test, model.predict(X_test)))

    values, counts = np.unique(y_train, return_counts=True)
    majority = values[np.argmax(counts)]
    trivial_score = float(np.mean(y_test == majority))

    result = {
        "dataset": "optical_digits",
        "metric": "accuracy",
        "reference_score": reference_score,
        "trivial_score": trivial_score,
        "model": "StandardScaler + SVC (RBF kernel, C=1.0, gamma='scale'), scikit-learn defaults",
        "train_rows": int(X_train.shape[0]),
        "test_rows": int(X_test.shape[0]),
        "train_sha256": sha256(train_path),
        "test_sha256": sha256(test_path),
        "runtime_seconds": round(time.perf_counter() - start, 3),
        "packages": {
            "python": sys.version.split()[0],
            "numpy": np.__version__,
            "pandas": pd.__version__,
            "scikit-learn": sklearn.__version__,
            "scipy": scipy.__version__,
        },
    }
    out = Path(__file__).resolve().parent / "result.json"
    out.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
