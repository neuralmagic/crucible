//! Captured HTML runs in an opaque iframe with no network or native bridge.

pub const ELEMENT: &str = "captured-html-preview";
pub const OBSCURED: &str = "captured-html-obscured";
pub const SUPPORTED: bool = cfg!(all(target_os = "macos", not(feature = "hot")));

#[cfg(all(target_os = "macos", not(feature = "hot")))]
mod macos;

#[cfg(all(target_os = "macos", not(feature = "hot")))]
pub use macos::install;

#[cfg(not(any(target_os = "macos", target_arch = "wasm32", feature = "hot")))]
pub fn install(app: rune::App<crate::App>) -> rune::App<crate::App> {
    app
}

#[cfg(any(all(target_os = "macos", not(feature = "hot")), test))]
fn document(source: &str) -> String {
    let escaped = source
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; \
         script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data: blob:; \
         font-src data:; frame-src about:; connect-src 'none'; form-action 'none'; base-uri 'none'\">\
         <style>html,body,iframe{{margin:0;width:100%;height:100%;border:0;display:block;overflow:hidden}}</style>\
         </head><body><iframe title=\"Captured report\" sandbox=\"allow-scripts\" srcdoc=\"{escaped}\"></iframe></body></html>"
    )
}

#[cfg(test)]
mod tests {
    use crate::html_preview::document;

    #[test]
    fn a_report_cannot_escape_its_opaque_frame_or_enable_network_access() {
        let source = r#"\"><script>fetch('https://example.com')</script><iframe src=\"file:///etc/passwd\">&"#;
        let page = document(source);
        assert_eq!(page.matches("<iframe").count(), 1);
        assert!(!page.contains("<script>"));
        assert!(page.contains("sandbox=\"allow-scripts\""));
        assert!(!page.contains("allow-same-origin"));
        assert!(page.contains("connect-src 'none'"));
        assert!(page.contains("default-src 'none'"));
        assert!(page.contains("frame-src about:"));
        assert!(page.contains("&lt;script&gt;"));
        assert!(page.contains("&amp;"));
    }
}
