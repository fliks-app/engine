// M1 placeholder: becomes the golden decide() harness; passes while fixtures/decide is empty.
#[test]
fn decide_fixtures() {
    insta::with_settings!({ allow_empty_glob => true }, {
        insta::glob!("../../../fixtures/decide", "*.json", |path| {
            let input: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            insta::assert_json_snapshot!(input);
        });
    });
}
