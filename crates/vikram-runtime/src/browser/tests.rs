use super::*;

const PAGE: &str = r#"<!doctype html>
<html><head><title>Example &amp; Co</title><style>body { color: rebeccapurple }</style></head>
<body>
  <!-- a comment that must never surface -->
  <h1>Hello <b>world</b></h1>
  <p>First para.</p><p>Second&nbsp;para.</p>
  <script>var x = "<p>not text</p>";</script>
  <a href="/one" class="x">One</a>
  <a class='two' href='https://example.com/two?q=1'>Two <em>links</em></a>
  <a name="anchor">no href here</a>
  <a href="">Empty href</a>
  <form action="/search" method="get">
    <input type="text" name="q" placeholder="Search the site">
    <input type="submit">
    <textarea name="note" placeholder="Say hi"></textarea>
    <select name="lang"><option value="en">English</option></select>
    <button type="submit" name="go">Go</button>
  </form>
  <input name="outside" type="hidden">
</body></html>"#;

// --- discovery ---------------------------------------------------------------------------

#[test]
fn the_missing_browser_message_names_the_override_variable() {
    assert!(MISSING_BROWSER.contains(ENV_VAR));
}

#[test]
fn an_environment_override_wins() {
    let found = resolve(
        |key| (key == ENV_VAR).then(|| "/custom/chrome".to_string()),
        |_| Some(PathBuf::from("/path/chromium")),
        |path| path == Path::new("/custom/chrome"),
    );
    assert_eq!(found, Some(PathBuf::from("/custom/chrome")));
}

#[test]
fn an_override_naming_a_program_is_looked_up_on_the_path() {
    let found = resolve(
        |_| Some("my-chromium".to_string()),
        |name| (name == "my-chromium").then(|| PathBuf::from("/opt/my-chromium/bin/my-chromium")),
        |_| false,
    );
    assert_eq!(
        found,
        Some(PathBuf::from("/opt/my-chromium/bin/my-chromium"))
    );
}

#[test]
fn a_stale_override_falls_through_to_the_path_search() {
    let found = resolve(
        |_| Some("/gone/chrome".to_string()),
        |name| (name == "chromium").then(|| PathBuf::from("/usr/bin/chromium")),
        |_| false,
    );
    assert_eq!(found, Some(PathBuf::from("/usr/bin/chromium")));
}

#[test]
fn an_absolute_candidate_is_found_when_the_path_has_nothing() {
    let found = resolve(
        |_| None,
        |_| None,
        |path| path == Path::new("/usr/bin/google-chrome"),
    );
    assert_eq!(found, Some(PathBuf::from("/usr/bin/google-chrome")));
}

#[test]
fn no_browser_at_all_resolves_to_nothing() {
    assert_eq!(resolve(|_| None, |_| None, |_| false), None);
}

#[test]
fn an_empty_override_is_ignored() {
    let found = resolve(
        |_| Some("   ".to_string()),
        |name| (name == "chromium").then(|| PathBuf::from("/usr/bin/chromium")),
        |_| false,
    );
    assert_eq!(found, Some(PathBuf::from("/usr/bin/chromium")));
}

#[test]
fn a_binary_that_does_not_exist_is_reported_without_hanging() {
    let err = navigate_with(
        Path::new("/nonexistent/vikram-chromium"),
        "https://example.com",
        500,
    )
    .unwrap_err();
    assert!(err.contains("cannot start"), "{err}");
}

#[test]
fn a_screenshot_from_a_missing_binary_fails_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("shot.png");
    let err = screenshot_with(
        Path::new("/nonexistent/vikram-chromium"),
        "https://example.com",
        &out,
        64,
        64,
        500,
    )
    .unwrap_err();
    assert!(err.contains("cannot start"), "{err}");
    assert!(!out.exists());
}

#[test]
fn headless_flags_are_shared_and_the_url_comes_last() {
    let args = browser_args(&["--dump-dom"], "https://example.com");
    assert_eq!(args[0], "--headless=new");
    assert_eq!(args[1], "--disable-gpu");
    assert_eq!(args[2], "--no-sandbox");
    assert_eq!(args[3], "--dump-dom");
    assert_eq!(args.last().unwrap(), "https://example.com");
}

// --- text extraction ---------------------------------------------------------------------

#[test]
fn extract_text_keeps_prose_and_drops_markup() {
    let text = extract_text(PAGE);
    assert!(text.contains("Hello world"), "{text}");
    assert!(text.contains("First para."), "{text}");
    assert!(text.contains("Second para."), "{text}");
    assert!(text.contains("Example & Co"), "{text}");
    assert!(!text.contains('<'), "{text}");
    assert!(!text.contains("&nbsp;"), "{text}");
    assert!(!text.contains("&amp;"), "{text}");
}

#[test]
fn extract_text_skips_scripts_styles_and_comments() {
    let text = extract_text(PAGE);
    assert!(!text.contains("rebeccapurple"), "{text}");
    assert!(!text.contains("var x"), "{text}");
    assert!(!text.contains("not text"), "{text}");
    assert!(
        !text.contains("a comment that must never surface"),
        "{text}"
    );
}

#[test]
fn extract_text_puts_block_elements_on_their_own_lines() {
    let text = extract_text("<div>one</div><div>two</div><br><span>three</span>");
    assert_eq!(text, "one\ntwo\nthree");
}

#[test]
fn extract_text_ignores_the_uppercase_doctype_chrome_emits() {
    // Chrome's `--dump-dom` starts every document with `<!DOCTYPE html>` in upper case.
    let text = extract_text("<!DOCTYPE html><html><body><p>hi</p></body></html>");
    assert_eq!(text, "hi");
}

#[test]
fn extract_text_decodes_numeric_entities() {
    assert_eq!(extract_text("<p>&#65;&#x42;</p>"), "AB");
}

#[test]
fn extract_text_leaves_unknown_entities_alone() {
    assert_eq!(extract_text("<p>a &bogus; b</p>"), "a &bogus; b");
}

#[test]
fn extract_text_on_a_fragment_without_tags_returns_the_text() {
    assert_eq!(extract_text("just words"), "just words");
}

#[test]
fn extract_text_survives_unterminated_markup() {
    assert_eq!(extract_text("<p>open"), "open");
    assert_eq!(extract_text("<p>open <!-- never closed"), "open");
}

// --- links -------------------------------------------------------------------------------

#[test]
fn extract_links_reads_both_quote_styles_and_strips_inner_tags() {
    let links = extract_links(PAGE);
    assert_eq!(links.len(), 2, "{links:?}");
    assert_eq!(links[0], ("/one".to_string(), "One".to_string()));
    assert_eq!(
        links[1],
        (
            "https://example.com/two?q=1".to_string(),
            "Two links".to_string()
        )
    );
}

#[test]
fn extract_links_ignores_anchors_without_a_target() {
    let dom = r#"<a name="top">Top</a><a href="a.html">A</a><a href="">B</a>"#;
    let links = extract_links(dom);
    assert_eq!(links, vec![("a.html".to_string(), "A".to_string())]);
}

#[test]
fn extract_links_does_not_mistake_another_attribute_for_the_target() {
    let dom = r#"<a data-href="/wrong" href="/right">Right</a>"#;
    assert_eq!(
        extract_links(dom),
        vec![("/right".to_string(), "Right".to_string())]
    );
}

#[test]
fn extract_links_ignores_links_inside_comments() {
    let dom = r#"<!-- <a href="/hidden">hidden</a> --><a href="/shown">shown</a>"#;
    assert_eq!(
        extract_links(dom),
        vec![("/shown".to_string(), "shown".to_string())]
    );
}

// --- forms -------------------------------------------------------------------------------

#[test]
fn extract_forms_describes_the_form_and_its_controls() {
    let forms = extract_forms(PAGE);
    assert!(
        forms.contains(&"form action=/search method=get".to_string()),
        "{forms:?}"
    );
    assert!(
        forms.contains(&"input type=text name=q placeholder=\"Search the site\"".to_string()),
        "{forms:?}"
    );
    assert!(
        forms.contains(&"input type=submit".to_string()),
        "{forms:?}"
    );
    assert!(
        forms.contains(&"textarea name=note placeholder=\"Say hi\"".to_string()),
        "{forms:?}"
    );
    assert!(forms.contains(&"select name=lang".to_string()), "{forms:?}");
    assert!(
        forms.contains(&"button type=submit name=go".to_string()),
        "{forms:?}"
    );
}

#[test]
fn extract_forms_lists_controls_outside_a_form_too() {
    let forms = extract_forms(PAGE);
    assert!(
        forms.contains(&"input type=hidden name=outside".to_string()),
        "{forms:?}"
    );
}

#[test]
fn extract_forms_on_a_page_without_forms_is_empty() {
    assert!(extract_forms("<p>nothing here</p>").is_empty());
}

#[test]
fn a_closing_form_tag_is_not_a_form() {
    assert!(extract_forms("</form>").is_empty());
}
