//! Port of `k8s.io/apiserver/pkg/warning`: handlers record warnings for the
//! current request (`warning.AddWarning(ctx, "", msg)`) and the server returns
//! them as `Warning: 299 - "<message>"` response headers, which `kubectl`
//! prints as `Warning: <message>`.

use std::sync::{Arc, Mutex};

tokio::task_local! {
    static WARNINGS: Arc<Mutex<Vec<String>>>;
}

/// Runs `f` with a fresh warning recorder; returns its output and the warnings.
pub async fn scope<F: std::future::Future>(f: F) -> (F::Output, Vec<String>) {
    let rec = Arc::new(Mutex::new(Vec::new()));
    let out = WARNINGS.scope(rec.clone(), f).await;
    let w = std::mem::take(&mut *rec.lock().unwrap());
    (out, w)
}

/// Records a warning for the current request (deduplicated, like `recorder.AddWarning`).
pub fn add(msg: impl Into<String>) {
    let msg = msg.into();
    let _ = WARNINGS.try_with(|w| {
        let mut w = w.lock().unwrap();
        if !w.contains(&msg) {
            w.push(msg);
        }
    });
}

/// `net/http.NewWarningHeader(299, "-", msg)`: the text is quoted, with `"` and `\` escaped.
pub fn header_value(msg: &str) -> String {
    let mut s = String::from("299 - \"");
    for c in msg.chars() {
        if c == '"' || c == '\\' {
            s.push('\\');
        }
        s.push(c);
    }
    s.push('"');
    s
}
