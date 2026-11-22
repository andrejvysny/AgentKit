//! Extraction: readability-style main content, fallback, charsets, JSON.

use agentkit_toolpacks_web::fetch::FetchedBody;
use agentkit_toolpacks_web::fetch::extract::extract;
use url::Url;

fn body(content_type: &str, charset: Option<&str>, bytes: &[u8]) -> FetchedBody {
    FetchedBody {
        final_url: Url::parse("https://example.com/docs/page").unwrap(),
        content_type: content_type.into(),
        charset: charset.map(str::to_owned),
        bytes: bytes.to_vec(),
        truncated_body: false,
    }
}

fn html(bytes: &[u8]) -> FetchedBody {
    body("text/html", None, bytes)
}

#[test]
fn article_keeps_main_and_drops_noise() {
    let page = extract(&html(include_bytes!("fixtures/fetch/article.html")));
    let md = &page.markdown;
    assert_eq!(page.title.as_deref(), Some("Tokio & Friends - Guide"));
    assert!(md.contains("# Getting started with Tokio"), "{md}");
    assert!(md.contains("## Installation"), "{md}");
    assert!(md.contains("**asynchronous runtime**"), "{md}");
    assert!(
        md.contains("- First item") && md.contains("- Second item"),
        "{md}"
    );
    assert!(
        md.contains("```") && md.contains("async fn main() {}"),
        "{md}"
    );
    for noise in [
        "NOISE_NAV_LINK",
        "NOISE_FOOTER_TEXT",
        "NOISE_SCRIPT_TEXT",
        "NOISE_FORM_TEXT",
        "NOISE_SITE_HEADER",
    ] {
        assert!(!md.contains(noise), "{noise} in {md}");
    }
}

#[test]
fn links_absolute_images_alt_unsafe_links_text() {
    let md = extract(&html(include_bytes!("fixtures/fetch/article.html"))).markdown;
    assert!(
        md.contains("[installation guide](https://example.com/guide/install)"),
        "{md}"
    );
    assert!(
        md.contains("[external notes](https://example.org/ext)"),
        "{md}"
    );
    assert!(md.contains("Scheduler diagram The scheduler"), "{md}");
    assert!(!md.contains("diagram.png") && !md.contains("!["), "{md}");
    assert!(
        md.contains("bad link") && !md.contains("javascript:"),
        "{md}"
    );
}

#[test]
fn js_only_page_falls_back_to_whole_body() {
    let page = extract(&html(include_bytes!("fixtures/fetch/js_only.html")));
    assert!(page.markdown.contains("Loading the application"));
    assert!(!page.markdown.contains("NOISE_JS"));
    assert_eq!(page.title.as_deref(), Some("App"));
}

#[test]
fn short_main_falls_back_to_whole_body() {
    let src =
        "<body><main><p>tiny</p></main><p>Outside the main element there is more text.</p></body>";
    let md = extract(&html(src.as_bytes())).markdown;
    assert!(
        md.contains("tiny") && md.contains("Outside the main element"),
        "{md}"
    );
}

#[test]
fn whole_body_strips_noise_tags_and_blank_runs() {
    let src = "<body><nav>NOISE_NAV</nav><header>NOISE_HDR</header><p>a</p><br><br><br><br><p>b</p><svg>NOISE_SVG</svg><footer>NOISE_FTR</footer></body>";
    let md = extract(&html(src.as_bytes())).markdown;
    assert!(!md.contains("NOISE"), "{md}");
    assert!(!md.contains("\n\n\n\n"), "{md:?}");
    assert!(md.contains('a') && md.contains('b'));
}

#[test]
fn windows_1250_meta_charset_is_decoded() {
    let page = extract(&html(include_bytes!("fixtures/fetch/slovak_cp1250.html")));
    assert!(
        page.markdown
            .contains("Príliš žltoučký kôň úpel ďábelské ódy"),
        "{}",
        page.markdown
    );
    assert!(page.markdown.contains("Ľudovít Štúr"));
    assert_eq!(page.title.as_deref(), Some("Žltý kôň"));
}

#[test]
fn header_charset_wins_over_meta_and_bad_utf8_is_lossy() {
    let bytes = include_bytes!("fixtures/fetch/slovak_cp1250.html");
    let page = extract(&body("text/html", Some("windows-1250"), bytes));
    assert!(page.markdown.contains("Ľudovít"));
    let lossy = extract(&body("text/plain", None, b"ok \xff\xfe end"));
    assert!(lossy.markdown.starts_with("ok ") && lossy.markdown.ends_with(" end"));
}

#[test]
fn json_is_pretty_printed_or_raw() {
    let pretty = extract(&body("application/json", None, br#"{"a":1,"b":[1,2]}"#)).markdown;
    assert!(pretty.contains("\n  \"a\": 1"), "{pretty}");
    let raw = extract(&body("application/json", None, b"{not json")).markdown;
    assert_eq!(raw, "{not json");
}

#[test]
fn plain_markdown_and_xml_pass_through() {
    for ct in ["text/plain", "text/markdown", "application/xml", "text/xml"] {
        let md = extract(&body(ct, None, b"# Title\n\n<a>raw</a>")).markdown;
        assert_eq!(md, "# Title\n\n<a>raw</a>", "{ct}");
    }
}

#[test]
fn title_is_clamped_to_200_chars() {
    let html = format!(
        "<html><head><title>{}</title></head><body><p>x</p></body></html>",
        "t".repeat(500)
    );
    let title = extract(&html_bytes(&html)).title.unwrap();
    assert_eq!(title.chars().count(), 201, "{title}");
    assert!(title.ends_with('…'));
}

fn html_bytes(s: &str) -> FetchedBody {
    html(s.as_bytes())
}

#[test]
fn deeply_nested_html_falls_back_to_plain_text() {
    let html = format!(
        "<html><title>Deep</title><body>{}needle &amp; hay{}</body></html>",
        "<div>".repeat(100_000),
        "</div>".repeat(100_000)
    );
    let page = extract(&html_bytes(&html));
    assert!(page.markdown.contains("needle & hay"), "{}", page.markdown);
    assert_eq!(page.title.as_deref(), Some("Deep"));
}

#[test]
fn plain_fallback_drops_script_and_tags() {
    let deep = "<div>".repeat(500);
    let html =
        format!("<script>var x='<div>';</script><style>a{{}}</style>{deep}one<br>two<p>three</p>");
    let md = extract(&html_bytes(&html)).markdown;
    assert_eq!(md, "one\n\ntwo\n\nthree");
}

#[test]
fn moderate_nesting_and_unclosed_paragraphs_still_use_markdown() {
    let html = format!(
        "<body>{}<h1>Head</h1>{}{}</body>",
        "<div>".repeat(50),
        "<p>para".repeat(1000),
        "</div>".repeat(50)
    );
    let md = extract(&html_bytes(&html)).markdown;
    assert!(md.contains("# Head"), "{md}");
}
