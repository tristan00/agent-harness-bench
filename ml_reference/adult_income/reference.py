import hashlib
import json
import sys
import time
from pathlib import Path

import numpy as np
import pandas as pd
import scipy
import sklearn
from sklearn.compose import ColumnTransformer
from sklearn.ensemble import HistGradientBoostingClassifier
from sklearn.metrics import roc_auc_score
from sklearn.pipeline import Pipeline
from sklearn.preprocessing import OrdinalEncoder

SEED = 0
COLUMNS = [
    "age", "workclass", "fnlwgt", "education", "education_num", "marital_status",
    "occupation", "relationship", "race", "sex", "capital_gain", "capital_loss",
    "hours_per_week", "native_country", "income",
]
CATEGORICAL = [
    "workclass", "education", "marital_status", "occupation", "relationship",
    "race", "sex", "native_country",
]
NUMERIC = ["age", "fnlwgt", "education_num", "capital_gain", "capital_loss", "hours_per_week"]


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def load(path, skiprows):
    df = pd.read_csv(
        path, header=None, names=COLUMNS, skiprows=skiprows, skipinitialspace=True,
        na_values="?", skip_blank_lines=True,
    )
    df = df.dropna(subset=["income"])
    y = (df["income"].str.strip().str.rstrip(".") == ">50K").astype(int).to_numpy()
    return df[CATEGORICAL + NUMERIC], y


def main():
    start = time.perf_counter()
    data_dir = Path(sys.argv[1])
    train_path = data_dir / "adult.data"
    test_path = data_dir / "adult.test"

    X_train, y_train = load(train_path, 0)
    X_test, y_test = load(test_path, 1)

    pre = ColumnTransformer(
        [("cat", OrdinalEncoder(handle_unknown="use_encoded_value", unknown_value=np.nan,
                                encoded_missing_value=np.nan), CATEGORICAL)],
        remainder="passthrough",
    )
    model = Pipeline([
        ("pre", pre),
        ("clf", HistGradientBoostingClassifier(
            categorical_features=[True] * len(CATEGORICAL) + [False] * len(NUMERIC),
            random_state=SEED,
        )),
    ])
    model.fit(X_train, y_train)
    proba = model.predict_proba(X_test)[:, 1]

    reference_score = float(roc_auc_score(y_test, proba))
    trivial_score = float(roc_auc_score(y_test, np.zeros(len(y_test))))

    result = {
        "dataset": "adult_income",
        "metric": "roc_auc",
        "reference_score": round(reference_score, 6),
        "trivial_score": trivial_score,
        "model": "sklearn HistGradientBoostingClassifier (default hyperparameters, native categorical handling via ordinal encoding, all 14 raw features, random_state=0)",
        "train_rows": int(len(y_train)),
        "test_rows": int(len(y_test)),
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
    out.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
