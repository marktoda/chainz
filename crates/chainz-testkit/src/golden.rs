//! Golden files: expected output kept as plain files under a crate's
//! `tests/golden/`. `CHAINZ_BLESS=1` rewrites them instead of comparing;
//! review the diff before committing.

use std::path::Path;

/// Compare `actual` with `<crate>/tests/golden/<name>`, or rewrite the file
/// when `CHAINZ_BLESS=1`: `golden!("listing/ls.txt", &output)`.
#[macro_export]
macro_rules! golden {
    ($name:expr, $actual:expr) => {
        $crate::golden::check(
            &::std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden"),
            $name,
            $actual,
        )
    };
}

/// What [`golden!`] calls: compare `actual` with `dir/name`.
#[track_caller]
pub fn check(dir: &Path, name: &str, actual: &str) {
    let path = dir.join(name);
    if std::env::var_os("CHAINZ_BLESS").is_some_and(|v| v == "1") {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e}; run with CHAINZ_BLESS=1 to create it",
            path.display()
        )
    });
    if expected == actual {
        return;
    }
    let first = expected
        .lines()
        .zip(actual.lines())
        .position(|(e, a)| e != a)
        .unwrap_or_else(|| expected.lines().count().min(actual.lines().count()));
    let window = |text: &str| -> String {
        text.lines()
            .enumerate()
            .skip(first.saturating_sub(3))
            .take(7)
            .map(|(i, l)| format!("{:>5} | {l}\n", i + 1))
            .collect()
    };
    panic!(
        "{name} differs from {} at line {} (CHAINZ_BLESS=1 rewrites it)\n--- expected\n{}--- actual\n{}",
        path.display(),
        first + 1,
        window(&expected),
        window(actual)
    );
}
