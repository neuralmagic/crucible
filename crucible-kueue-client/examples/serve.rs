//! Serves the test crucible-kueue for clients in other languages: prints one JSON line with its
//! URL and a task token, then runs until stdin closes.

#[path = "../tests/common/mod.rs"]
mod common;

fn main() {
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    let url = runtime.block_on(common::serve());
    println!(
        "{}",
        serde_json::json!({"url": url, "token": common::task_token()})
    );
    let mut line = String::new();
    while std::io::stdin().read_line(&mut line).is_ok_and(|n| n > 0) {
        line.clear();
    }
}
