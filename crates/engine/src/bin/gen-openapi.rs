use utoipa::OpenApi;

// Writes the file itself so regenerating it never needs to read or redirect into it.
fn main() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../openapi.json");
    let json = fliks_engine::ApiDoc::openapi().to_pretty_json().unwrap();
    std::fs::write(path, json + "\n").unwrap();
}
